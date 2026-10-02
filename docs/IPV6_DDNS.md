# IPv6 and DDNS networking

The XPARQ mainnet DDNS bootstrap endpoint is:

```text
xparqnode.duckdns.org:6677
```

DDNS keeps the hostname stable while its AAAA record tracks the bootstrap
node's public IPv6 address. Connecting peers need working IPv6 connectivity;
DDNS does not provide an IPv4-to-IPv6 relay.

## Connect to the bootstrap node

After building the mainnet node, run from the repository root:

```bash
./target/release/node run \
  --p2p '[::]:6677' \
  --rpc '[::]:6666' \
  --peer xparqnode.duckdns.org:6677
```

`[::]:6677` listens on IPv6 interfaces. The hostname is resolved on each
connection attempt, including reconnects. DNS caches can delay discovery of a
changed address. An existing TCP connection cannot move to the new IPv6 address;
the node must reconnect.

The standard TCP supervisor limits outbound workers (dialing, synchronizing,
or connected) to 8, with at most 2 concurrent dial/handshake attempts. TCP
connection attempts share a 10-second budget across the resolved addresses;
DNS resolution uses the system resolver's timeout. Handshake I/O retains its
60-second timeout. Dial and handshake run before taking the chain sync lock.
Failed sessions release their worker slot during reconnect backoff so other
candidate peers can be tried.

The endpoint is for the standard mainnet TCP transport. Testnet/devnet use
different networks and ports. The experimental litep2p transport additionally
requires a peer identity; this endpoint alone is not a litep2p bootstrap entry.

## Accept incoming IPv6 connections

Use the node computer's globally routable IPv6 address, with brackets around
the IP literal:

```bash
./target/release/node run \
  --p2p '[::]:6677' \
  --rpc '[::]:6666' \
  --public-addr '[YOUR_PUBLIC_IPV6]:6677' \
  --peer xparqnode.duckdns.org:6677
```

Replace `YOUR_PUBLIC_IPV6` with the actual address. Allow inbound TCP port 6677
on the computer and router IPv6 firewalls. The examples also bind RPC to all
IPv6 interfaces at `[::]:6666`; restrict TCP port 6666 to authorized clients
with firewall rules. Use `http://[::1]:6666` for local RPC requests or
`http://xparqnode.duckdns.org:6666` for the DDNS node if its RPC is enabled and
reachable. `[::]` is a bind address, not a client destination.
Permit essential ICMPv6 traffic, including Packet Too Big,
so IPv6 path MTU discovery can work.

`--public-addr` is your own reachable address; `--peer` identifies another node.
`[::]` is a listener address and cannot be advertised as a public endpoint.
Direct public IPv6 does not require the IPv4 NAT mapping option
`--nat-traversal`.

## Changing IPv6 addresses

For a node with a changing ISP prefix, configure a DDNS updater to publish the
node computer's current public IPv6 address in its hostname's AAAA record.
Update after address changes and periodically. Use a stable interface address
within the current prefix where possible, rather than a rotating temporary
privacy address. Keep the DDNS API token private.

Use your own DDNS hostname for public advertisement. For the operator of
`xparqnode.duckdns.org`, the command is:

```bash
./target/release/node run \
  --p2p '[::]:6677' \
  --rpc '[::]:6666' \
  --public-addr xparqnode.duckdns.org:6677
```

Other operators must use their own hostname. Do not advertise someone else's
bootstrap hostname as your node's address.

Both `--peer` and `--public-addr` accept IP literals or fully qualified DNS
hostnames with a port. Peer storage preserves hostnames across restarts, and
standard TCP discovery shares hostnames. Successful hostname connections are
stored under the hostname rather than the transient resolved IP. Reconnects
resolve DNS again.

The node refreshes its advertised hostname's resolved public IP fallbacks every
60 seconds and replaces old addresses. DNS failure clears the IP fallbacks while
keeping the hostname. Updating the AAAA record externally is still required;
the node does not call DuckDNS or hold its API token. With a hostname configured,
you do not need to restart the node when the DDNS IPv6 address changes. An
explicit IP literal remains fixed until you restart with a new value.

Discovery keeps the existing string-list wire format. Older nodes can read the
IP entries but ignore hostname entries; they do not gain dynamic DDNS support.
Existing IP-only peer records remain readable without a storage reset.
Automatically discovered endpoints are restricted to public resolved addresses
on every dial, including after a DNS change. Explicit `--peer` endpoints can
still connect to private or loopback addresses for local networks.

These features apply to the standard TCP transport. The experimental litep2p
transport retains its separate peer address and identity format.

## Check DNS and connectivity

On a system with `dig` and `nc` installed:

```bash
dig AAAA xparqnode.duckdns.org +short
nc -6 -vz -w 5 xparqnode.duckdns.org 6677
```

The DNS lookup should return the current public IPv6 address. A successful TCP
check confirms port reachability; the node still validates the XPARQ handshake
and chain before accepting the peer.

References: [DuckDNS update API](https://www.duckdns.org/spec.jsp) and
[IPv6 Path MTU Discovery (RFC 8201)](https://www.rfc-editor.org/rfc/rfc8201.html).
