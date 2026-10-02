use std::net::ToSocketAddrs;

use super::util::*;
use super::*;

impl RunConfig {
    pub(super) fn parse(args: &[String]) -> Result<Self, String> {
        let mut database = PathBuf::from(default_database());
        let mut p2p_listen = default_p2p_listen().to_string();
        let mut rpc_listen = default_rpc_listen().to_string();
        let mut peers = Vec::new();
        let mut miner = None;
        let mut public_addr = None;
        let mut nat_traversal = false;
        #[cfg(feature = "litep2p-devnet")]
        let mut litep2p = false;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--data" => {
                    index += 1;
                    database = PathBuf::from(args.get(index).ok_or("missing value for --data")?);
                }
                "--p2p" => {
                    index += 1;
                    p2p_listen = args.get(index).ok_or("missing value for --p2p")?.clone();
                }
                "--rpc" => {
                    index += 1;
                    rpc_listen = args.get(index).ok_or("missing value for --rpc")?.clone();
                }
                "--peer" => {
                    index += 1;
                    peers.push(args.get(index).ok_or("missing value for --peer")?.clone());
                }
                "--miner" => {
                    index += 1;
                    miner = Some(parse_address(
                        args.get(index).ok_or("missing value for --miner")?,
                    )?);
                }
                "--public-addr" => {
                    index += 1;
                    public_addr = Some(
                        args.get(index)
                            .ok_or("missing value for --public-addr")?
                            .parse()
                            .map_err(|_| "invalid --public-addr endpoint")?,
                    );
                }
                "--nat-traversal" => nat_traversal = true,
                #[cfg(feature = "litep2p-devnet")]
                "--litep2p" => litep2p = true,
                option => return Err(format!("unknown node run option `{option}`")),
            }
            index += 1;
        }
        Ok(Self {
            database,
            p2p_listen,
            rpc_listen,
            peers,
            miner,
            public_addr,
            nat_traversal,
            #[cfg(feature = "litep2p-devnet")]
            litep2p,
        })
    }
}

pub(super) fn configure_public_address(config: &RunConfig) -> Result<(), String> {
    if config.public_addr.is_some() && config.nat_traversal {
        return Err("use either --public-addr or --nat-traversal, not both".into());
    }
    if let Some(address) = config.public_addr.clone() {
        if !address.is_admissible() {
            return Err(
                "--public-addr must be a public IP or DNS hostname with a non-zero port".into(),
            );
        }
        set_advertised_peer(Some(address.clone()))?;
        if matches!(address, PeerAddress::Dns { .. }) {
            thread::spawn(move || {
                loop {
                    match address.to_string().to_socket_addrs() {
                        Ok(addresses) => {
                            let public: Vec<_> = addresses
                                .filter(is_admissible_discovered_peer)
                                .take(MAX_DISCOVERED_PEERS)
                                .collect();
                            if let Err(error) = replace_advertised_dns_ips(&address, &public) {
                                eprintln!("node: refresh DDNS advertisement: {error}");
                            }
                        }
                        Err(error) => {
                            // Do not continue announcing potentially obsolete IP fallbacks.
                            let _ = replace_advertised_dns_ips(&address, &[]);
                            eprintln!("node: resolve advertised DDNS endpoint: {error}");
                        }
                    }
                    thread::sleep(Duration::from_secs(60));
                }
            });
        }
    }
    if !config.nat_traversal {
        return Ok(());
    }
    let listener: SocketAddr = config
        .p2p_listen
        .parse()
        .map_err(|_| "--nat-traversal requires a numeric P2P listen address")?;
    let mapping = crate::nat::map_tcp_listener(listener, DEFAULT_NAT_LEASE)?;
    if !is_admissible_discovered_peer(&mapping.public_addr) {
        return Err("NAT gateway returned a non-public address".into());
    }
    set_advertised_peer(Some(mapping.public_addr.into()))?;
    println!(
        "nat: mapped public_addr={} lease_secs={}",
        mapping.public_addr,
        mapping.lease.as_secs()
    );
    thread::spawn(move || {
        loop {
            thread::sleep(mapping.lease / 2);
            match crate::nat::map_tcp_listener(listener, DEFAULT_NAT_LEASE) {
                Ok(refreshed) if is_admissible_discovered_peer(&refreshed.public_addr) => {
                    if let Err(error) = set_advertised_peer(Some(refreshed.public_addr.into())) {
                        eprintln!("node: update NAT public address: {error}");
                    }
                }
                Ok(_) => eprintln!("node: NAT refresh returned a non-public address"),
                Err(error) => eprintln!("node: NAT mapping refresh failed: {error}"),
            }
        }
    });
    Ok(())
}

pub(super) struct PublicAdvertisement {
    endpoint: PeerAddress,
    resolved: Vec<SocketAddr>,
}

impl PublicAdvertisement {
    fn replace_resolved(&mut self, addresses: &[SocketAddr]) {
        self.resolved = addresses
            .iter()
            .copied()
            .filter(is_admissible_discovered_peer)
            .take(MAX_DISCOVERED_PEERS - 1)
            .collect();
        self.resolved.sort();
        self.resolved.dedup();
    }

    fn addresses(&self) -> Vec<String> {
        let mut addresses = vec![self.endpoint.to_string()];
        if matches!(self.endpoint, PeerAddress::Dns { .. }) {
            addresses.extend(self.resolved.iter().map(ToString::to_string));
        }
        addresses
    }
}

pub(super) fn set_advertised_peer(address: Option<PeerAddress>) -> Result<(), String> {
    *ADVERTISED_PEER
        .get_or_init(|| RwLock::new(None))
        .write()
        .map_err(|_| "advertised peer lock is poisoned")? =
        address.map(|endpoint| PublicAdvertisement {
            endpoint,
            resolved: Vec::new(),
        });
    Ok(())
}

fn replace_advertised_dns_ips(
    endpoint: &PeerAddress,
    addresses: &[SocketAddr],
) -> Result<(), String> {
    let mut advertisement = ADVERTISED_PEER
        .get_or_init(|| RwLock::new(None))
        .write()
        .map_err(|_| "advertised peer lock is poisoned")?;
    if let Some(current) = advertisement.as_mut() {
        if current.endpoint == *endpoint {
            current.replace_resolved(addresses);
        }
    }
    Ok(())
}

pub(super) fn advertised_peer_addresses() -> Result<Vec<String>, String> {
    let advertisement = ADVERTISED_PEER
        .get_or_init(|| RwLock::new(None))
        .read()
        .map_err(|_| "advertised peer lock is poisoned")?;
    Ok(advertisement
        .as_ref()
        .map_or_else(Vec::new, PublicAdvertisement::addresses))
}

pub(super) fn print_network_info() -> Result<(), String> {
    println!("genesis: {}", hex::encode(EXPECTED_GENESIS_HASH.0));
    println!(
        "chain_spec: {}",
        hex::encode(chain_spec_hash().map_err(|error| error.to_string())?.0)
    );
    println!("p2p_protocol: {P2P_PROTOCOL_VERSION}");
    println!("pow: {}", kernel::consensus::POW_ALGORITHM);
    println!("difficulty: {}", kernel::consensus::DIFFICULTY_ALGORITHM);
    Ok(())
}

pub(super) fn print_help() {
    #[cfg(feature = "litep2p-devnet")]
    println!(
        "node run --litep2p [--data PATH] [--p2p ADDRESS] [--rpc ADDRESS] [--peer ADDRESS@PEER_ID]... [--miner ADDRESS]"
    );
    println!(
        "node run [--data PATH] [--p2p ADDRESS] [--rpc ADDRESS] [--peer ADDRESS]... [--miner ADDRESS] [--public-addr ADDRESS | --nat-traversal]\nnode network [data-dir] [listen-address] [peer-address...]\nnode litep2p [data-dir] [listen-address] [peer-address...] (requires litep2p-devnet feature)\nnode rpc [data-dir] [listen-address]\nnode p2p-listen [data-dir] [listen-address]\nnode peer [data-dir] <peer-address>\nnode info\nnode check [data-dir]\nnode account [data-dir] <address>\nnode mempool [data-dir]\nnode mine-block [data-dir] <miner-address>\nnode submit-transaction [data-dir] <transaction-hex>\nnode submit-block [data-dir] <block-hex>\nnode version"
    );
}

pub(super) fn database_path(path: Option<&str>) -> PathBuf {
    PathBuf::from(path.unwrap_or(default_database()))
}

#[cfg(feature = "mainnet")]
pub(super) fn default_p2p_listen() -> &'static str {
    "0.0.0.0:6677"
}

#[cfg(feature = "mainnet")]
pub(super) fn default_rpc_listen() -> &'static str {
    "127.0.0.1:6666"
}
#[cfg(feature = "testnet")]
pub(super) fn default_rpc_listen() -> &'static str {
    "127.0.0.1:16666"
}
#[cfg(feature = "devnet")]
pub(super) fn default_rpc_listen() -> &'static str {
    "127.0.0.1:26666"
}
#[cfg(feature = "testnet")]
pub(super) fn default_p2p_listen() -> &'static str {
    "0.0.0.0:16677"
}
#[cfg(feature = "devnet")]
pub(super) fn default_p2p_listen() -> &'static str {
    "0.0.0.0:26677"
}

#[cfg(feature = "mainnet")]
pub(super) fn default_database() -> &'static str {
    "./data/mainnet"
}
#[cfg(feature = "testnet")]
pub(super) fn default_database() -> &'static str {
    "./data/testnet"
}
#[cfg(feature = "devnet")]
pub(super) fn default_database() -> &'static str {
    "./data/devnet"
}

#[cfg(test)]
mod ddns_tests {
    use super::*;

    #[test]
    fn public_addr_accepts_ddns_and_preserves_ipv6_literals() {
        for endpoint in ["xparqnode.duckdns.org:6677", "[2606:4700::1111]:6677"] {
            let config = RunConfig::parse(&["--public-addr".into(), endpoint.into()]).unwrap();
            let address = config.public_addr.unwrap();
            assert_eq!(address.to_string(), endpoint);
            assert!(address.is_admissible());
        }
    }

    #[test]
    fn refresh_replaces_old_ipv6_and_keeps_hostname() {
        let mut advertisement = PublicAdvertisement {
            endpoint: "xparqnode.duckdns.org:6677".parse().unwrap(),
            resolved: Vec::new(),
        };
        advertisement.replace_resolved(&["[2606:4700::1111]:6677".parse().unwrap()]);
        advertisement.replace_resolved(&[
            "[2606:4700::1001]:6677".parse().unwrap(),
            "[::1]:6677".parse().unwrap(),
            "[2606:4700::1001]:6677".parse().unwrap(),
        ]);
        assert_eq!(
            advertisement.addresses(),
            ["xparqnode.duckdns.org:6677", "[2606:4700::1001]:6677"]
        );
        advertisement.replace_resolved(&[]);
        assert_eq!(advertisement.addresses(), ["xparqnode.duckdns.org:6677"]);
    }
}
