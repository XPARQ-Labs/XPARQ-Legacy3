# XPARQ

**XPARQ** is an experimental post-quantum Layer 1 blockchain written in Rust.

The project is organized around a small set of components:

- `crypto` — cryptographic primitives and post-quantum signature support
- `kernel` — consensus, ledger, transactions, native coin, and asset rules
- `runtime` — node, P2P networking, synchronization, storage, mining, and RPC
- `wallet` — local wallet and CLI
- `docs` — protocol and RPC documentation
- `depend` — vendored or project-pinned dependencies

> [!WARNING]
> XPARQ is under active development. Protocol rules, database formats, networking, wallet formats, and APIs may change. Do not use funds or keys that you cannot afford to lose.

## Requirements

XPARQ currently requires:

- Git
- Rust **1.90 or newer**
- Cargo
- A supported 64-bit operating system

On Debian/Ubuntu, install common build tools:

```bash
sudo apt update
sudo apt install -y git curl build-essential pkg-config
```

Install Rust with `rustup` if it is not already installed:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Verify the installation:

```bash
rustc --version
cargo --version
```

## Clone the repository

```bash
git clone https://github.com/XPARQ-Labs/XPARQ.git
cd XPARQ
```

## Build

Build the node and wallet in release mode:

```bash
cargo build --release -p node -p wallet
```

The binaries will be available at:

```text
target/release/node
target/release/wallet
```

For development builds:

```bash
cargo build -p node -p wallet
```

## Run a node

### Mainnet

Mainnet is the default feature.

After building:

```bash
./target/release/node
```

This is equivalent to starting the automatic node runtime with the default mainnet configuration.

Default mainnet settings:

| Setting | Default |
| --- | --- |
| Data directory | `./data/mainnet` |
| P2P listen | `0.0.0.0:6677` |
| RPC listen | `127.0.0.1:6666` |
| Mining | Disabled |

A normal startup will print information similar to:

```text
database: ./data/mainnet
rpc: http://127.0.0.1:6666
outbound_peers: 0
mining: disabled
```

Stop the node with:

```text
Ctrl+C
```

### Run with an explicit peer

To connect to another XPARQ node:

```bash
./target/release/node run \
  --peer <IP:PORT>
```

Example:

```bash
./target/release/node run \
  --peer 203.0.113.10:6677
```

Multiple peers can be provided:

```bash
./target/release/node run \
  --peer 203.0.113.10:6677 \
  --peer 203.0.113.11:6677
```

### Custom data directory

```bash
./target/release/node run \
  --data /path/to/xparq-data
```

Example:

```bash
./target/release/node run \
  --data "$HOME/.xparq/mainnet"
```

### Custom P2P and RPC addresses

```bash
./target/release/node run \
  --p2p 0.0.0.0:6677 \
  --rpc 127.0.0.1:6666
```

The RPC interface should normally remain bound to localhost unless you deliberately place it behind appropriate network controls.

## Run a mining node

Mining requires an XPARQ address.

First create a wallet:

```bash
./target/release/wallet new
```

Or open the interactive wallet:

```bash
./target/release/wallet
```

The wallet will display an address after creation.

Start the node with mining enabled:

```bash
./target/release/node run \
  --miner <XPARQ_ADDRESS>
```

Example:

```bash
./target/release/node run \
  --miner YOUR_XPARQ_ADDRESS
```

You can combine mining with peers:

```bash
./target/release/node run \
  --peer <PEER_IP:6677> \
  --miner <XPARQ_ADDRESS>
```

When mining is active, the node prints:

```text
mining: enabled on the local canonical tip
```

Mining rewards are assigned to the address supplied through `--miner`.

## Public node

A node that accepts incoming P2P connections should expose its P2P port.

Default mainnet P2P port:

```text
6677/TCP
```

If using UFW:

```bash
sudo ufw allow 6677/tcp
```

Do **not** expose the RPC port to the public internet unless you understand the security implications.

The default RPC interface is intentionally bound to:

```text
127.0.0.1:6666
```

### Advertise a public address

If the machine has a directly reachable public IP:

```bash
./target/release/node run \
  --public-addr <PUBLIC_IP:6677>
```

Example:

```bash
./target/release/node run \
  --public-addr 203.0.113.20:6677
```

### Automatic NAT traversal

XPARQ can attempt to map its P2P listener through a compatible NAT gateway:

```bash
./target/release/node run \
  --nat-traversal
```

`--public-addr` and `--nat-traversal` are mutually exclusive.

## RPC

The default mainnet RPC endpoint is:

```text
http://127.0.0.1:6666
```

Check node status:

```bash
curl -s http://127.0.0.1:6666/status
```

Pretty-print with `jq`:

```bash
curl -s http://127.0.0.1:6666/status | jq
```

Check the fee policy:

```bash
curl -s http://127.0.0.1:6666/fee-policy | jq
```

Get recent blocks:

```bash
curl -s http://127.0.0.1:6666/blocks/latest | jq
```

Query a block by height:

```bash
curl -s http://127.0.0.1:6666/block/0 | jq
```

Query a balance:

```bash
curl -s "http://127.0.0.1:6666/balance/<XPARQ_ADDRESS>" | jq
```

### RPC documentation

While the node is running, open:

```text
http://127.0.0.1:6666/docs
```

The raw OpenAPI document is available at:

```text
http://127.0.0.1:6666/openapi.json
```

## Wallet

Start the interactive wallet:

```bash
./target/release/wallet
```

The current interactive menu includes:

```text
XPARQ Wallet
1. Create Wallet
2. Import Wallet
3. Show Address
4. Show Balance
5. Transaction History
6. UTXO
7. Transfer
8. Consolidate UTXOs
9. Explorer
10. Assets
11. Exit
```

### Create a wallet

Interactive:

```bash
./target/release/wallet
```

Or CLI:

```bash
./target/release/wallet new
```

The default wallet file is:

```text
wallet.json
```

A custom wallet path can be supplied:

```bash
./target/release/wallet new \
  --wallet my-wallet.json
```

By default, wallet creation uses the `mldsa44` signature account.

Available signature accounts currently include:

```text
mldsa44
mldsa65
mldsa87
```

Example:

```bash
./target/release/wallet new \
  --account mldsa44 \
  --wallet wallet.json
```

> [!IMPORTANT]
> Back up the mnemonic shown during wallet creation. Anyone with access to the mnemonic may be able to control the wallet.

### Show wallet address

```bash
./target/release/wallet address \
  --wallet wallet.json
```

### Show balance

With the mainnet RPC running locally:

```bash
./target/release/wallet balance \
  --wallet wallet.json \
  --rpc 127.0.0.1:6666
```

## Network modes

XPARQ currently defines separate Cargo features for:

- `mainnet`
- `testnet`
- `devnet`

Only one network mode should be selected for a build.

### Mainnet

Mainnet is enabled by default:

```bash
cargo build --release -p node -p wallet
```

Defaults:

```text
P2P: 0.0.0.0:6677
RPC: 127.0.0.1:6666
Data: ./data/mainnet
```

### Testnet

Build:

```bash
cargo build --release \
  -p node \
  -p wallet \
  --no-default-features \
  --features testnet
```

Defaults:

```text
P2P: 0.0.0.0:16677
RPC: 127.0.0.1:16666
Data: ./data/testnet
```

Run:

```bash
./target/release/node
```

### Devnet

Build:

```bash
cargo build --release \
  -p node \
  -p wallet \
  --no-default-features \
  --features devnet
```

Defaults:

```text
P2P: 0.0.0.0:26677
RPC: 127.0.0.1:26666
Data: ./data/devnet
```

Run:

```bash
./target/release/node
```

#### Experimental litep2p devnet transport

Install `protoc` (the Protocol Buffers compiler) before building `litep2p`.
This transport uses its own protocol and port; legacy TCP peers cannot connect to it.

```bash
cargo build -p node --no-default-features --features litep2p-devnet
./target/debug/node litep2p ./data/devnet-litep2p 127.0.0.1:27677
```

For the normal node with RPC and optional mining, use `node run --litep2p`
with `--data`, `--p2p`, `--rpc`, and `--peer ADDRESS@PEER_ID` as needed.

The node prints its persistent litep2p peer ID. To connect another devnet node,
pass `ADDRESS@PEER_ID`, for example:

```bash
./target/debug/node litep2p ./data/devnet-litep2p-2 127.0.0.1:27678 \
  127.0.0.1:27677@<first-node-peer-id>
```

The litep2p transport validates the genesis and chain-spec hashes during
notification handshake. It verifies a peer's header branch and cumulative
work, downloads matching blocks, and applies the same fork-choice and reorg
logic as the legacy transport. It also relays new blocks and mempool transactions.
Its devnet protocol names are `/xparq/devnet/blocks/2` and
`/xparq/devnet/announce/2`.

## Node commands

Show available commands:

```bash
./target/release/node --help
```

Current node commands include:

```text
node run
node network
node rpc
node p2p-listen
node peer
node info
node check
node account
node mempool
node mine-block
node submit-transaction
node submit-block
node version
```

### Network information

```bash
./target/release/node info
```

This prints network-level identifiers such as:

- genesis hash
- chain specification hash
- P2P protocol version
- proof-of-work algorithm
- difficulty algorithm

### Check database

```bash
./target/release/node check
```

Or specify a data directory:

```bash
./target/release/node check /path/to/xparq-data
```

### Local ledger snapshots

The node writes a ledger snapshot every 1,000 blocks and after a sync that
applies at least 1,000 blocks. It retains the two newest snapshots. On startup,
the node verifies a snapshot against the local canonical block log, restores
ledger state and rollback journals, then replays blocks after the snapshot.
If no stored snapshot matches, it replays the chain from genesis. Version 1
snapshots remain readable; new snapshots use the smaller version 2 format.
The canonical block log remains required for startup and reorganization.

### Inspect mempool

```bash
./target/release/node mempool
```

### Inspect an account

```bash
./target/release/node account \
  ./data/mainnet \
  <XPARQ_ADDRESS>
```

## Useful development commands

### Combined coin and asset spend

`wallet asset-transfer` now places the asset transfer and its XPQ fee payment in
one signed spend. The same wallet must own both sets of inputs. To transfer XPQ
to a recipient in that transaction as well, provide both `--coin-to ADDRESS`
and `--coin-amount XPQ`. Asset registration, mint, and burn still use their
separate asset-call authorization and XPQ payment authorization.
The old two-authorization asset-transfer variant has been removed; only the
combined spend format is accepted after the chain reset.

All XPQ spend intents now carry `SpendCharges { miner_fee }`. The miner fee is
credited to the block miner as a separate UTXO; it is no longer encoded as a
normal coin output. The protocol burn remains the verified difference between
XPQ inputs, address outputs, and the miner fee. This changes transaction bytes
and derived output IDs. Start upgraded nodes with fresh chain storage after the
planned chain reset.
Coin outputs encode the recipient address directly; the obsolete block-miner
recipient tag is no longer part of a spend output.

This transaction format changes consensus. Nodes that have not been upgraded
will reject combined spends; deploy it with a coordinated network upgrade.

Check the complete workspace:

```bash
cargo check --workspace
```

Run workspace tests:

```bash
cargo test --workspace
```

Build optimized binaries:

```bash
cargo build --release -p node -p wallet
```

Check formatting:

```bash
cargo fmt --all -- --check
```

Run Clippy:

```bash
cargo clippy --workspace --all-targets
```

## Project structure

```text
XPARQ/
├── crypto/       Cryptographic primitives
├── kernel/       Consensus and ledger rules
├── runtime/      Node runtime
├── wallet/       Wallet and CLI
├── docs/         Protocol and RPC documentation
├── depend/       Vendored dependencies
├── Cargo.toml
├── Cargo.lock
├── LICENSE
└── SECURITY.md
```

## Security

XPARQ is experimental software and has not reached a stable production release.

If you discover a security issue, avoid publishing exploit details before maintainers have had an opportunity to investigate it.

See [`SECURITY.md`](SECURITY.md) for the project's security policy.

## License

XPARQ is licensed under the [MIT License](LICENSE).

Copyright (c) 2026 XPARQ Network contributors.
