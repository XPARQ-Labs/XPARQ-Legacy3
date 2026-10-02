use std::{
    collections::BTreeMap,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const PEERS_KEY: &str = "peers";
pub const MAX_DISCOVERED_PEERS: usize = 128;

/// A stable peer endpoint. DNS is resolved only when dialing, never on decode.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PeerAddress {
    Ip(SocketAddr),
    Dns { hostname: String, port: u16 },
}

impl From<SocketAddr> for PeerAddress {
    fn from(address: SocketAddr) -> Self {
        Self::Ip(address)
    }
}

impl FromStr for PeerAddress {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Ok(address) = value.parse::<SocketAddr>() {
            return Ok(Self::Ip(address));
        }
        let (hostname, port) = value.rsplit_once(':').ok_or("peer requires host:port")?;
        let hostname = hostname
            .strip_suffix('.')
            .unwrap_or(hostname)
            .to_ascii_lowercase();
        let port = port.parse::<u16>().map_err(|_| "invalid peer port")?;
        if hostname.len() > 253
            || !hostname.contains('.')
            || hostname.parse::<IpAddr>().is_ok()
            || hostname
                .split('.')
                .all(|label| label.bytes().all(|c| c.is_ascii_digit()))
            || hostname.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            })
        {
            return Err("invalid peer hostname (use a fully qualified DNS name)".into());
        }
        Ok(Self::Dns { hostname, port })
    }
}

impl fmt::Display for PeerAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ip(address) => address.fmt(f),
            Self::Dns { hostname, port } => write!(f, "{hostname}:{port}"),
        }
    }
}

impl PeerAddress {
    pub fn is_admissible(&self) -> bool {
        match self {
            Self::Ip(address) => is_admissible_discovered_peer(address),
            Self::Dns { hostname, port } => {
                *port != 0
                    && hostname != "localhost"
                    && !hostname.ends_with(".localhost")
                    && !hostname.ends_with(".local")
                    && !hostname.ends_with(".internal")
                    && !hostname.ends_with(".lan")
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerRecord {
    pub address: String,
    pub successes: u32,
    pub failures: u32,
    pub last_success_unix: Option<u64>,
    pub cooldown_until_unix: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct PeerFile {
    peers: Vec<PeerRecord>,
}

#[derive(Default)]
pub struct PeerStore {
    peers: BTreeMap<PeerAddress, PeerRecord>,
}

impl PeerStore {
    pub fn load(database: &Path) -> Result<Self, String> {
        let contents = match crate::storage::auxiliary_get(database, PEERS_KEY)? {
            Some(contents) => contents,
            None => return Ok(Self::default()),
        };
        let decoded: PeerFile = serde_json::from_slice(&contents)
            .map_err(|error| format!("decode peer store: {error}"))?;
        let mut peers = BTreeMap::new();
        for record in decoded.peers.into_iter().take(MAX_DISCOVERED_PEERS) {
            let Ok(address) = record.address.parse::<PeerAddress>() else {
                continue;
            };
            if address.is_admissible() {
                peers.insert(address, record);
            }
        }
        Ok(Self { peers })
    }

    pub fn addresses(&self) -> Vec<PeerAddress> {
        let now = unix_time();
        let mut addresses: Vec<_> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.cooldown_until_unix.is_none_or(|until| until <= now))
            .map(|(address, _)| address.clone())
            .collect();
        // Prefer stable DNS identities over potentially stale IP fallback records.
        addresses.sort_by_key(|address| matches!(address, PeerAddress::Ip(_)));
        addresses
    }

    pub fn relay_addresses(&self) -> Vec<String> {
        self.peers
            .iter()
            .filter(|(_, peer)| peer.last_success_unix.is_some())
            .take(MAX_DISCOVERED_PEERS)
            .map(|(address, _)| address.to_string())
            .collect()
    }

    pub fn record_success(&mut self, address: PeerAddress) {
        if !address.is_admissible() {
            return;
        }
        if !self.peers.contains_key(&address) && self.peers.len() >= MAX_DISCOVERED_PEERS {
            return;
        }
        let record = self.record(address);
        record.successes = record.successes.saturating_add(1);
        record.failures = 0;
        record.last_success_unix = Some(unix_time());
        record.cooldown_until_unix = None;
    }

    pub fn record_failure(&mut self, address: PeerAddress, malicious: bool) {
        if !address.is_admissible() {
            return;
        }
        if !self.peers.contains_key(&address) && self.peers.len() >= MAX_DISCOVERED_PEERS {
            return;
        }
        let record = self.record(address);
        record.failures = record.failures.saturating_add(1);
        let exponent = record.failures.min(8);
        let ordinary = 10_u64.saturating_mul(1_u64 << exponent);
        let delay = if malicious {
            3_600
        } else {
            ordinary.min(1_800)
        };
        record.cooldown_until_unix = Some(unix_time().saturating_add(delay));
    }

    pub fn insert_discovered(&mut self, address: PeerAddress) -> bool {
        if !address.is_admissible()
            || self.peers.contains_key(&address)
            || self.peers.len() >= MAX_DISCOVERED_PEERS
        {
            return false;
        }
        self.peers.insert(
            address.clone(),
            PeerRecord {
                address: address.to_string(),
                successes: 0,
                failures: 0,
                last_success_unix: None,
                cooldown_until_unix: None,
            },
        );
        true
    }

    pub fn save(&self, database: &Path) -> Result<(), String> {
        let encoded = serde_json::to_vec_pretty(&PeerFile {
            peers: self.peers.values().cloned().collect(),
        })
        .map_err(|error| format!("encode peer store: {error}"))?;
        crate::storage::auxiliary_put(database, PEERS_KEY, &encoded)
    }

    fn record(&mut self, address: PeerAddress) -> &mut PeerRecord {
        self.peers
            .entry(address.clone())
            .or_insert_with(|| PeerRecord {
                address: address.to_string(),
                successes: 0,
                failures: 0,
                last_success_unix: None,
                cooldown_until_unix: None,
            })
    }
}

pub fn is_admissible_discovered_peer(address: &SocketAddr) -> bool {
    address.port() != 0
        && match address.ip() {
            IpAddr::V4(ip) => admissible_ipv4(ip),
            IpAddr::V6(ip) => admissible_ipv6(ip),
        }
}

fn admissible_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || a == 0
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn admissible_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return admissible_ipv4(ipv4);
    }
    let segments = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_rejects_non_public_addresses() {
        for address in [
            "0.0.0.0:6677",
            "127.0.0.1:6677",
            "10.0.0.1:6677",
            "100.64.0.1:6677",
            "192.0.2.1:6677",
            "[::1]:6677",
            "[fc00::1]:6677",
            "[2001:db8::1]:6677",
        ] {
            assert!(!is_admissible_discovered_peer(&address.parse().unwrap()));
        }
        assert!(is_admissible_discovered_peer(
            &"8.8.8.8:6677".parse().unwrap()
        ));
    }

    #[test]
    fn dns_endpoints_are_canonical_and_reject_malformed_names() {
        let address: PeerAddress = "XPARQNode.DuckDNS.org.:6677".parse().unwrap();
        assert_eq!(address.to_string(), "xparqnode.duckdns.org:6677");
        assert!(address.is_admissible());
        for value in [
            "localhost:6677",
            "https://node.example:6677",
            "node..example:6677",
            "-node.example:6677",
            "node_.example:6677",
            "node.example:65536",
            "999.1.1.1:6677",
        ] {
            assert!(value.parse::<PeerAddress>().is_err(), "{value}");
        }
        for value in ["node.local:6677", "node.localhost:6677", "node.example:0"] {
            assert!(!value.parse::<PeerAddress>().unwrap().is_admissible());
        }
    }

    #[test]
    fn storage_preserves_dns_identity_and_loads_existing_ip_records() {
        let database = std::env::temp_dir().join(format!(
            "xparq-ddns-store-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let legacy = br#"{"peers":[{"address":"8.8.8.8:6677","successes":1,"failures":0,"last_success_unix":1,"cooldown_until_unix":null}]}"#;
        crate::storage::auxiliary_put(&database, PEERS_KEY, legacy).unwrap();
        let mut store = PeerStore::load(&database).unwrap();
        assert_eq!(
            store.addresses(),
            ["8.8.8.8:6677".parse::<PeerAddress>().unwrap()]
        );
        let dns: PeerAddress = "xparqnode.duckdns.org:6677".parse().unwrap();
        store.insert_discovered(dns.clone());
        store.record_success(dns.clone());
        store.save(&database).unwrap();
        let mut loaded = PeerStore::load(&database).unwrap();
        assert_eq!(loaded.addresses().first(), Some(&dns));
        assert!(loaded.relay_addresses().contains(&dns.to_string()));
        loaded.record_failure(dns.clone(), true);
        loaded.save(&database).unwrap();
        assert!(
            !PeerStore::load(&database)
                .unwrap()
                .addresses()
                .contains(&dns)
        );
        std::fs::remove_dir_all(database).unwrap();
    }

    #[test]
    fn successful_peers_cannot_grow_storage_beyond_the_limit() {
        let mut store = PeerStore::default();
        for index in 0..MAX_DISCOVERED_PEERS + 10 {
            store.record_success(format!("node{index}.example:6677").parse().unwrap());
        }
        assert_eq!(store.addresses().len(), MAX_DISCOVERED_PEERS);
    }

    #[test]
    fn malicious_failure_applies_a_long_cooldown() {
        let address = "8.8.8.8:6677".parse().unwrap();
        let mut store = PeerStore::default();
        store.record_failure(address, true);
        assert!(store.addresses().is_empty());
    }
}
