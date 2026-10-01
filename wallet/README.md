# XPARQ Wallet

`wallet/` contains the reusable `wallet` library and the `wallet`
executable. It never opens node storage and communicates through HTTP RPC.

```bash
cargo build --release --locked -p wallet
./target/release/wallet
./target/release/wallet --help
```

The interactive menu supports wallet creation and restoration, balances,
canonical history, UTXO tracking and consolidation, XPQ sends, Program assets,
and block exploration.

## Security

The wallet file contains recovery and private signing material. It is checked
for internal consistency when loaded, created atomically, and given owner-only
permissions on Unix. Back up the mnemonic offline and never commit, upload, or
share the wallet file. A miner needs only the public payout address. Keep plain
HTTP RPC on loopback or a trusted private network.

## XPQ transactions

Signed transactions are submitted automatically to `/transaction`. Use
`--offline` to print signed canonical transaction bytes without submitting them.
RPC is still needed when selecting inputs or calculating fees from node state.

```bash
./target/release/wallet sign-spend --to ADDRESS --amount 1 --rpc 127.0.0.1:6666
./target/release/wallet consolidate --wallet wallet.json --rpc 127.0.0.1:6666
```

Without explicit inputs, the wallet selects available `/account/{address}`
UTXOs and creates change. Consolidation merges the selected XPQ UTXOs into one
self-owned output. Consensus validates it as an ordinary transaction, so its
canonical bytes still incur archival burn and a miner fee.

The miner fee is node policy. The protocol burn separately covers canonical
transaction history and positive net state growth. Consumed Coin UTXOs offset
new Coin UTXOs for state growth but do not erase historical transaction bytes.

## Extension asset program

Panduan langkah demi langkah: [Penggunaan ProgramCall](../docs/PROGRAM_CALL_USAGE.md).

Assets use the extension Program registry and `program-*` commands. Amounts use
8 decimal places. XPQ fee inputs, change, miner fee and exact protocol burn are
selected automatically. Commands need a running node for balances and quotes.

```sh
wallet program-register --name Gold --max-supply 100 --initial-mint 40 --wallet wallet.json --rpc 127.0.0.1:6666
wallet program-mint --asset CONTRACT --to ADDRESS --amount 20
wallet program-transfer --asset CONTRACT --to ADDRESS --amount 15
wallet program-burn --asset CONTRACT --amount 5
wallet program-consolidate --asset CONTRACT
wallet program-info --asset CONTRACT
wallet program-balance --asset CONTRACT --address ADDRESS
```

`--fixed-supply` disables further minting at registration. `--offline` prints
canonical signed transaction hex without submitting it; RPC is still needed to
select XPQ/share inputs and quote state growth. Transfer/burn consume at most
256 shares per call. Consolidation merges up to 256 shares per invocation and
requires at least two shares. Wait for pending asset operations to confirm
before submitting a dependent operation.

Interactive mode has a **Program Assets** menu. Normal balance and history show
extension holdings and operations; asset-only recipients also receive history
entries even when they receive no XPQ.

RPC reads: `/program/asset/{asset}` and
`/program/asset/{asset}/balance/{address}`. `/account` and `/balance` expose a
separate `program_assets` array; `/explorer/address` contains aggregate balances
without share lists. `/explorer/transaction` decodes Program asset instructions.
`POST /program/quote` reads a signed canonical Program transaction and returns
extension state growth in bytes without admitting or applying the transaction.
Its quote reflects canonical state; final submission revalidates against mempool
state and can reject a stale quote.

### Integration verification

```sh
cargo build -p node -p wallet --bins
cargo test -p wallet --test program_e2e -- --ignored --test-threads=1
```

This test uses real wallet/node binaries and temporary redb storage for register,
mint, transfer, burn, consolidation, balances, recipient history and restart.
