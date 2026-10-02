# ProgramCall integration status

For CLI instructions, see [Program asset commands](../wallet/README.md#extension-asset-program).

## Current structure after legacy asset removal

| Layer | Responsibility |
| --- | --- |
| `kernel::monetary::coin` | XPQ amounts, contract identity, share IDs and outputs |
| `kernel::ledger::utxo::UtxoSet` | XPQ UTXOs only: amount and owner, indexed by CoinShare |
| `LedgerState.coin` | XPQ mined/burned counters and live supply |
| `LedgerState.extensions` | Canonical extension state, including Program asset records and shares |
| `AuthorizedTransaction` | Signed Program call with an XPQ coin transition |
| `StateRollbackJournal` | Coin spend journal and optional extension journal |

The legacy `LedgerState.assets`, asset shares in `UtxoSet`, `AssetIntent`,
`AuthorizedAssetTransaction`, and `Spend::Combined` are removed. The standalone
`kernel::monetary::asset` and `kernel::transaction::asset` implementations are
removed as well. Asset definitions and execution live in `extension`.

The XPQ supply invariant remains `total_mined - total_burned = sum(live coin
UTXOs)`. Program asset supply is checked independently against extension shares.

## Native XPQ transfer boundary

`kernel::monetary::coin` owns the XPQ amount, contract, share and output types.
The XPQ contract identity remains zero. The kernel owns coin UTXOs and monetary
counters; the extension does not import the kernel or receive a mutable ledger.

Wallet spends and consolidation use `ProgramId::XPQ` (0), method `TRANSFER` (1).
Transfer data is the authenticated envelope's `payment` field; the call payload
is empty. Kernel preparation checks signatures, ownership, duplicate inputs,
conservation and required fees/burn before execution.

`extension::coin_program::execute_transfer` controls input consumption, output
creation, the miner-fee output and protocol burn through a restricted `CoinHost`.
A private kernel adapter implements that capability and records rollback data.
The kernel commits Program transactions only after execution and supply checks
succeed. Raw UTXO insertion/removal is unavailable to other crates.

`CoinTransition` carries the authenticated coin inputs, outputs, and miner fee
for a Program call. The native `Spend` transaction variant has been removed.
Asset-call XPQ funding uses the same executor. Block emission and rollback remain
kernel consensus operations. The transaction encoding and derived output IDs
change, so this revision requires fresh chain storage.

## Implemented: payment preparation and canonical state

`kernel::program::prepare_program_transaction` authenticates the entire Program
transaction against the chain and height, previews extension asset execution,
and validates ownership and conservation of its XPQ payment inputs.

Required protocol burn uses the existing transaction archival burn plus state
growth burn. Extension growth is the positive difference between canonical
extension state sizes before and after the call (bytes). Net created coin UTXOs,
including an optional miner-fee output, use the existing coin state weight.
State shrinkage earns no refund. Miner fee is explicitly deducted separately.
The coin-transition signer must equal the call signer.
The signature binds call, payment, chain and principal authorization role.
Preparation does not mutate ledger state; consensus uses its result for Program validation.

`LedgerState.extensions` is canonically serialized, included in the application
state root, and checked for asset supply consistency alongside XPQ supply.
The empty ledger retains the zero root. Records and shares are deterministically
ordered by BTreeMap. Changes to either affect the root.

## Compatibility

The reset baseline uses chain spec version **1**; storage schema is **7**.
Canonical state bytes and
nonempty state roots changed even while the extension is empty. The canonical
genesis block remains unchanged. Use fresh compatible chain storage after the
planned reset; existing databases are rejected, not automatically migrated.
Frozen mainnet execution vectors and chain identity fixtures were regenerated.
Returning the chain spec version to 1 does not restore the previous protocol:
the chain-spec hash commits to the current transaction and state formats.
All participating nodes must rebuild with the same code and use fresh storage.
Legacy asset balances are not imported into the new chain.

## Implemented: atomic execution and rollback

`LedgerState::apply_program_transaction` revalidates the signed envelope against
current state, applies payment and extension operation on a cloned state, checks
supply invariants, and commits only on success. Its journal records the coin
changes and prior values of changed extension records/shares. The extension journal is Borsh encoded
with ledger snapshots; no second database is introduced.

Rollback applies extension and coin journals on a clone, so a corrupt coin
journal cannot partially restore the extension. Block rollback additionally
checks the parent state root before committing. Schema 6 reflects coin-only
UTXO storage and Program-only asset journals. Extension journals store only
changed records/shares, ordered by key,
rather than duplicating the full asset state for every transaction.

## Consensus activation

Program transactions are active in normal transaction validation, mempool state
simulation, block execution and canonical replay. `ValidatedTransaction::Program`
uses the ledger extension state and the current height. The former test-only
execution policy and `ProgramNotActive` rejection have been removed.

Legacy kernel assets, asset transactions and combined spends have been removed.
The kernel UTXO set holds XPQ only; assets live exclusively in the extension
registry. Legacy assets are not migrated because the chain will be reset.

Tests cover every opcode in committed blocks, serialized snapshot restoration,
full replay, rollback to every prior state, failed multi-call atomicity and corrupt
journal rejection. The node integration test submits a signed Program transaction
through RPC, mines it, restarts from redb and runs chain verification.

## Wallet and RPC

Wallet library `sign_program_call` authorizes call and payment together. CLI
`program-register`, `program-mint`, `program-transfer`, `program-burn`,
`program-consolidate`, `program-info` and `program-balance` provide normal flows,
including the interactive Program Assets menu. All asset amounts use 8 decimals.
Fees converge against signed canonical transaction size and consensus state
preview from `POST /program/quote`.

Account and balance responses expose `program_assets`; the legacy `assets`
field and `/asset/` routes have been removed. Extension metadata/balance routes
use `/program/asset/`.
Explorer transaction details decode asset instructions and XPQ payments. Address
indexes include mint/transfer asset recipients, including transfers of no XPQ.
Older address indexes are rebuilt when their Program index marker is absent.
Explorer address summaries omit share lists; history carries compact operation
and raw-unit amount summaries rather than full opcode payloads.

CLI integration tests cover register/mint/transfer/burn/consolidate, automatic
fees, asset-only recipient history, offline transaction output, canonical mining
and redb restart. They require building node binaries and are run explicitly.

## Verification after removal

The workspace tests and the five node network integration tests passed after
legacy removal. Frozen mainnet vectors now cover signed on-chain Program
register, mint, transfer and burn transactions, with replay and rollback.
The explicit wallet CLI lifecycle test also passed, including consolidation,
asset-only recipient history, offline output and redb restart.

To repeat the checks:

```bash
cargo check --workspace --all-targets --offline
cargo test --workspace --offline
cargo build -p node -p wallet --bins --offline
cargo test -p wallet --test program_e2e --offline -- --ignored --test-threads=1
cargo test -p node --test network_e2e --offline program_lifecycle_gossips_across_three_nodes_and_rolls_back_on_reorg -- --nocapture
git diff --check
```

Node and CLI integration tests open localhost RPC/P2P ports and need an
environment that permits local socket binding. The network suite covers block
gossip, coin transaction gossip, fork synchronization, DDNS discovery/restart,
and Program submission/restart. The three-node Program lifecycle test submits
register, mint, transfer, burn and consolidation at alternating network endpoints,
checks mempool propagation before mining on another node, and compares asset
metadata and shares after each confirmed block. It also checks restart recovery
and rollback of four Program operations to a stronger fork, including removal of
orphan transactions from the canonical explorer index.

## Next

Extend invalid Program transaction and deeper-fork coverage before release.
GUI wallet/asset pages remain deferred.

The isolated `AuthorizedProgramCall` bridge is for standalone extension tests;
on-chain calls must use the envelope that also authorizes their XPQ payment.
