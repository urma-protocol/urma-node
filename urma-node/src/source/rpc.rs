//! Bounded JSON-RPC to the optional local Litecoin Core. Every call runs
//! against a caller-owned deadline (connect included), so a stopped or hung
//! Core costs a caller at most that bound and never wedges a worker or the
//! control API.
use crate::Error;
use serde_json::{Value, json};
use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// 16M hex characters (the parent admission cap) plus envelope.
const RESPONSE_LIMIT: usize = 17 * 1024 * 1024;

/// One budget shared by every RPC of an operation (cookie verification plus
/// the real call), so the operation as a whole stays within it.
#[derive(Clone, Copy)]
pub(super) struct Deadline {
    at: Instant,
    budget: Duration,
}

impl Deadline {
    pub(super) fn after(budget: Duration) -> Self {
        Self {
            at: Instant::now() + budget,
            budget,
        }
    }

    fn left(&self, method: &str) -> Result<Duration, Error> {
        let left = self.at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(self.timed_out(method));
        }
        Ok(left)
    }

    fn timed_out(&self, method: &str) -> Error {
        Error::CommitSource(format!(
            "local RPC {method} timed out within {} ms",
            self.budget.as_millis()
        ))
    }

    fn transport(&self, method: &str, error: std::io::Error) -> Error {
        match error.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => self.timed_out(method),
            _ => Error::CommitSource(format!("local RPC {method}: {error}")),
        }
    }
}

/// A JSON-RPC answer: the result, or Core's own error object.
pub(super) enum Answer {
    Value(Value),
    Failed { code: i64, message: String },
}

pub(super) struct Rpc {
    address: SocketAddr,
    authorization: String,
}

impl Rpc {
    /// `url` must be `http://<loopback IP>:<port>/`; `cookie` authenticates.
    pub(super) fn new(url: &str, cookie: &[u8]) -> Result<Self, Error> {
        let origin = url
            .strip_prefix("http://")
            .map(|rest| rest.strip_suffix('/').unwrap_or(rest))
            .ok_or_else(|| invalid("RPC requires a local HTTP origin and cookie authentication"))?;
        let address: SocketAddr = origin
            .parse()
            .map_err(|_| invalid("RPC URL must be http://<loopback IP>:<port>/"))?;
        if !address.ip().is_loopback() {
            return Err(invalid("RPC must use a loopback IP"));
        }
        let cookie = std::str::from_utf8(cookie)
            .map_err(|_| Error::CommitSource("RPC cookie is not UTF-8".into()))?
            .trim();
        Ok(Self {
            address,
            authorization: format!("Basic {}", base64(cookie.as_bytes())),
        })
    }

    /// The result of `method`, with Core's error object as an error.
    pub(super) fn value(
        &self,
        method: &str,
        params: Value,
        deadline: Deadline,
    ) -> Result<Value, Error> {
        match self.call(method, params, deadline)? {
            Answer::Value(value) => Ok(value),
            Answer::Failed { code, message } => Err(Error::CommitSource(format!(
                "JSON-RPC {method} error {code}: {message}"
            ))),
        }
    }

    /// One call within `deadline`: connecting (at most CONNECT_TIMEOUT),
    /// writing the request and reading the reply all count against it.
    pub(super) fn call(
        &self,
        method: &str,
        params: Value,
        deadline: Deadline,
    ) -> Result<Answer, Error> {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "1.0", "id": "urma-node", "method": method, "params": params
        }))?;
        let connect = deadline.left(method)?.min(CONNECT_TIMEOUT);
        let mut stream = TcpStream::connect_timeout(&self.address, connect)
            .map_err(|error| Error::CommitSource(format!("local RPC unreachable: {error}")))?;
        let head = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.address,
            self.authorization,
            body.len()
        );
        stream.set_write_timeout(Some(deadline.left(method)?))?;
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(&body))
            .map_err(|error| deadline.transport(method, error))?;
        let reply = read_reply(&mut stream, deadline, method)?;
        parse(&reply, method)
    }
}

/// Read until the declared Content-Length is complete, or until EOF when the
/// reply declares none; a Core that keeps the socket open cannot hold us past
/// the reply itself.
fn read_reply(stream: &mut TcpStream, deadline: Deadline, method: &str) -> Result<Vec<u8>, Error> {
    let mut reply = Vec::new();
    let mut chunk = [0; 16 * 1024];
    let mut expected = None;
    loop {
        if let Some(total) = expected {
            if reply.len() >= total {
                reply.truncate(total);
                return Ok(reply);
            }
        }
        stream.set_read_timeout(Some(deadline.left(method)?))?;
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(reply),
            Ok(read) => {
                if reply.len() + read > RESPONSE_LIMIT {
                    return Err(Error::CommitSource(format!(
                        "local RPC {method} reply exceeds its limit"
                    )));
                }
                reply.extend_from_slice(&chunk[..read]);
                if expected.is_none() {
                    expected = declared_length(&reply, method)?;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(deadline.transport(method, error)),
        }
    }
}

/// Header plus body length once the header is complete and declares one.
fn declared_length(reply: &[u8], method: &str) -> Result<Option<usize>, Error> {
    let Some(split) = header_end(reply) else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&reply[..split])
        .map_err(|_| Error::CommitSource(format!("local RPC {method}: malformed HTTP reply")))?;
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            let length: usize = value.trim().parse().map_err(|_| {
                Error::CommitSource(format!("local RPC {method}: invalid Content-Length"))
            })?;
            return Ok(Some(split + 4 + length));
        }
    }
    Ok(None)
}

fn header_end(reply: &[u8]) -> Option<usize> {
    reply.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Core answers RPC errors with a JSON error object (HTTP 500/404) and
/// authentication failures with an empty 401.
fn parse(reply: &[u8], method: &str) -> Result<Answer, Error> {
    let malformed = || Error::CommitSource(format!("local RPC {method}: malformed HTTP reply"));
    let split = header_end(reply).ok_or_else(malformed)?;
    let head = std::str::from_utf8(&reply[..split]).map_err(|_| malformed())?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(malformed)?;
    if head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding") && value.trim() != "identity"
        })
    }) {
        return Err(Error::CommitSource(format!(
            "local RPC {method}: encoded reply unsupported"
        )));
    }
    let Ok(mut body) = serde_json::from_slice::<Value>(&reply[split + 4..]) else {
        return Err(Error::CommitSource(format!(
            "local RPC {method}: HTTP {status}"
        )));
    };
    let error = body["error"].take();
    if !error.is_null() {
        return Ok(Answer::Failed {
            code: error["code"].as_i64().ok_or_else(malformed)?,
            message: error["message"].as_str().unwrap_or_default().to_owned(),
        });
    }
    if status != 200 {
        return Err(Error::CommitSource(format!(
            "local RPC {method}: HTTP {status}"
        )));
    }
    Ok(Answer::Value(body["result"].take()))
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let triple = group.iter().enumerate().fold(0u32, |acc, (index, byte)| {
            acc | u32::from(*byte) << (16 - 8 * index)
        });
        for index in 0..4 {
            if index <= group.len() {
                let sextet = (triple >> (18 - 6 * index)) & 0x3f;
                encoded.push(char::from(ALPHABET[usize::try_from(sextet).unwrap_or(0)]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in vectors {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }
}
