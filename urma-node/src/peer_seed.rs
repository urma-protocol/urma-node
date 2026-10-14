//! `/dnsaddr` seeds are resolved here instead of inside the DNS transport,
//! which dials TXT records in whatever order the resolver returns them. The
//! records are dialed QUIC first (§3.1): behind a TCP proxy (Fly) only a QUIC
//! connection shows the seed our real mapping, so AutoNAT has a candidate.
use crate::{
    Error,
    peer_address::{AddressKind, parse, quic_first},
    peer_gossip::is_public,
};
use hickory_resolver::TokioResolver;
use libp2p::{Multiaddr, multiaddr::Protocol};
use std::time::Duration;

/// How `/dnsaddr` names resolve: the system resolver, or a fixed table of
/// TXT records per name (tests).
#[derive(Clone, Debug, Default)]
pub enum Names {
    #[default]
    System,
    Fixed(Vec<(String, Vec<Multiaddr>)>),
}

impl Names {
    /// Records dialed per seed at most, one at a time.
    pub const RECORDS: usize = 4;
    const LOOKUP_TIME: Duration = Duration::from_secs(5);

    /// What to dial for `address`, in order: a `/dnsaddr` seed's dialable
    /// records (those ending in its `/p2p`, when it names one) QUIC first;
    /// any other address as is.
    pub(crate) fn expand(&self, address: &Multiaddr) -> Result<Vec<Multiaddr>, Error> {
        let Some(Protocol::Dnsaddr(name)) = address.iter().next() else {
            return Ok(vec![address.clone()]);
        };
        let suffix: Multiaddr = address.iter().skip(1).collect();
        let mut records = self.lookup(&name)?;
        records.retain(|record| record.ends_with(&suffix) && dialable(record));
        let mut records = quic_first(records);
        records.truncate(Self::RECORDS);
        match records.is_empty() {
            true => Err(Error::Transport(format!("{address}: no dnsaddr records"))),
            false => Ok(records),
        }
    }

    fn lookup(&self, name: &str) -> Result<Vec<Multiaddr>, Error> {
        match self {
            Self::System => system(name),
            Self::Fixed(table) => Ok(table
                .iter()
                .filter(|(known, _)| known == name)
                .flat_map(|(_, records)| records.iter().cloned())
                .collect()),
        }
    }
}

/// A TXT record is answered by whoever controls the name: only a direct
/// address on a public IPv4 host is dialed. Loopback, private, IPv6, named,
/// circuit and nested `/dnsaddr` records are dropped.
fn dialable(record: &Multiaddr) -> bool {
    let Ok(shape) = parse(record) else {
        return false;
    };
    shape.kind == AddressKind::Direct
        && matches!(shape.bare.iter().next(), Some(Protocol::Ip4(_)))
        && is_public(&shape.bare)
}

/// One TXT lookup of `_dnsaddr.<name>` through the system configuration, on
/// a runtime of its own: the caller is a blocking discovery thread.
fn system(name: &str) -> Result<Vec<Multiaddr>, Error> {
    let failed =
        |error: &dyn std::fmt::Display| Error::Transport(format!("dnsaddr {name}: {error}"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| failed(&error))?;
    runtime.block_on(async {
        let resolver = TokioResolver::builder_tokio()
            .map_err(|error| failed(&error))?
            .build();
        let lookup = resolver.txt_lookup(format!("_dnsaddr.{name}."));
        let txt = tokio::time::timeout(Names::LOOKUP_TIME, lookup)
            .await
            .map_err(|error| failed(&error))?
            .map_err(|error| failed(&error))?;
        let mut records = Vec::new();
        for record in txt.iter() {
            for data in record.txt_data() {
                match parse_txt(data) {
                    Some(address) => records.push(address),
                    None => tracing::debug!(%name, "skipped a malformed dnsaddr record"),
                }
            }
        }
        Ok(records)
    })
}

/// `dnsaddr=<multiaddr>`; anything else is not a dnsaddr record.
fn parse_txt(data: &[u8]) -> Option<Multiaddr> {
    std::str::from_utf8(data)
        .ok()?
        .strip_prefix("dnsaddr=")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{Names, parse_txt};
    use libp2p::{Multiaddr, PeerId};

    fn address(text: &str) -> Multiaddr {
        text.parse().unwrap()
    }

    #[test]
    fn seed_records_are_dialed_quic_first_and_capped() {
        let (one, two) = (PeerId::random(), PeerId::random());
        let records = vec![
            address(&format!("/ip4/11.0.0.1/tcp/28332/p2p/{one}")),
            address(&format!("/ip4/11.0.0.1/udp/28332/quic-v1/p2p/{one}")),
            address(&format!("/ip4/12.0.0.1/tcp/28332/p2p/{two}")),
            address(&format!("/ip4/12.0.0.1/udp/28332/quic-v1/p2p/{two}")),
            address(&format!("/ip4/13.0.0.1/tcp/28332/p2p/{two}")),
        ];
        let names = Names::Fixed(vec![("seed.test".into(), records.clone())]);
        let expanded = names.expand(&address("/dnsaddr/seed.test")).unwrap();
        assert_eq!(
            expanded,
            vec![
                records[1].clone(),
                records[3].clone(),
                records[0].clone(),
                records[2].clone()
            ]
        );
        let named = address(&format!("/dnsaddr/seed.test/p2p/{two}"));
        assert_eq!(
            names.expand(&named).unwrap(),
            vec![records[3].clone(), records[2].clone(), records[4].clone()],
            "a seed naming its peer dials only that peer's records"
        );
        assert!(names.expand(&address("/dnsaddr/other.test")).is_err());
        let literal = address("/ip4/11.0.0.1/tcp/1");
        assert_eq!(names.expand(&literal).unwrap(), vec![literal]);
    }

    #[test]
    fn only_public_ipv4_direct_records_are_dialed() {
        let (peer, relay) = (PeerId::random(), PeerId::random());
        let kept = address(&format!("/ip4/11.0.0.1/udp/28332/quic-v1/p2p/{peer}"));
        let dropped = [
            format!("/ip4/127.0.0.1/tcp/28332/p2p/{peer}"),
            format!("/ip4/10.0.0.1/tcp/28332/p2p/{peer}"),
            format!("/ip4/192.168.1.1/udp/28332/quic-v1/p2p/{peer}"),
            format!("/ip4/100.64.0.1/tcp/28332/p2p/{peer}"),
            format!("/ip6/2001:4860::1/tcp/28332/p2p/{peer}"),
            format!("/dns4/seed.example/tcp/28332/p2p/{peer}"),
            format!("/dnsaddr/other.test/p2p/{peer}"),
            format!("/ip4/12.0.0.1/tcp/28332/p2p/{relay}/p2p-circuit/p2p/{peer}"),
            format!("/ip4/11.0.0.1/tcp/0/p2p/{peer}"),
        ];
        let mut records: Vec<Multiaddr> = dropped.iter().map(|text| address(text)).collect();
        records.push(kept.clone());
        let names = Names::Fixed(vec![("seed.test".into(), records)]);
        assert_eq!(
            names.expand(&address("/dnsaddr/seed.test")).unwrap(),
            vec![kept]
        );
        let hostile = Names::Fixed(vec![("seed.test".into(), vec![address(&dropped[0])])]);
        assert!(
            hostile.expand(&address("/dnsaddr/seed.test")).is_err(),
            "a seed with no dialable record is a failed lookup"
        );
    }

    #[test]
    fn only_dnsaddr_txt_records_parse() {
        assert_eq!(
            parse_txt(b"dnsaddr=/ip4/1.2.3.4/udp/1/quic-v1"),
            Some(address("/ip4/1.2.3.4/udp/1/quic-v1"))
        );
        assert_eq!(parse_txt(b"v=spf1 -all"), None);
        assert_eq!(parse_txt(b"dnsaddr=nonsense"), None);
    }
}
