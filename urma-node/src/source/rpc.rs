//! Bounded JSON-RPC to the optional local Litecoin Core. Every call has a
//! connect timeout and a total deadline, so a stopped or hung Core costs a
//! caller at most that bound and never wedges a worker or the control API.
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
        budget: Duration,
    ) -> Result<Value, Error> {
        match self.call(method, params, budget)? {
            Answer::Value(value) => Ok(value),
            Answer::Failed { code, message } => Err(Error::CommitSource(format!(
                "JSON-RPC {method} error {code}: {message}"
            ))),
        }
    }

    /// One call: at most CONNECT_TIMEOUT to connect, then `budget` in total
    /// for writing the request and reading the whole reply.
    pub(super) fn call(
        &self,
        method: &str,
        params: Value,
        budget: Duration,
    ) -> Result<Answer, Error> {
        let deadline = Instant::now() + budget;
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "1.0", "id": "urma-node", "method": method, "params": params
        }))?;
        let mut stream = TcpStream::connect_timeout(&self.address, CONNECT_TIMEOUT)
            .map_err(|error| Error::CommitSource(format!("local RPC unreachable: {error}")))?;
        let head = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.address,
            self.authorization,
            body.len()
        );
        stream.set_write_timeout(Some(left(deadline, method, budget)?))?;
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(&body))
            .map_err(|error| transport(method, budget, error))?;
        let reply = read_reply(&mut stream, deadline, method, budget)?;
        parse(&reply, method)
    }
}

fn read_reply(
    stream: &mut TcpStream,
    deadline: Instant,
    method: &str,
    budget: Duration,
) -> Result<Vec<u8>, Error> {
    let mut reply = Vec::new();
    let mut chunk = [0; 16 * 1024];
    loop {
        stream.set_read_timeout(Some(left(deadline, method, budget)?))?;
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(reply),
            Ok(read) => {
                if reply.len() + read > RESPONSE_LIMIT {
                    return Err(Error::CommitSource(format!(
                        "local RPC {method} reply exceeds its limit"
                    )));
                }
                reply.extend_from_slice(&chunk[..read]);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(transport(method, budget, error)),
        }
    }
}

/// Core answers RPC errors with a JSON error object (HTTP 500/404) and
/// authentication failures with an empty 401.
fn parse(reply: &[u8], method: &str) -> Result<Answer, Error> {
    let malformed = || Error::CommitSource(format!("local RPC {method}: malformed HTTP reply"));
    let split = reply
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(malformed)?;
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

fn left(deadline: Instant, method: &str, budget: Duration) -> Result<Duration, Error> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(timed_out(method, budget));
    }
    Ok(left)
}

fn transport(method: &str, budget: Duration, error: std::io::Error) -> Error {
    match error.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => timed_out(method, budget),
        _ => Error::CommitSource(format!("local RPC {method}: {error}")),
    }
}

fn timed_out(method: &str, budget: Duration) -> Error {
    Error::CommitSource(format!(
        "local RPC {method} timed out after {} ms",
        budget.as_millis()
    ))
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
