# XPARQ Security Hardening Roadmap

## Scope

This roadmap prioritizes safeguards for consensus execution, state transitions,
rollback, reorganization, and node resource use. It describes planned work;
items are complete only after their acceptance criteria pass. Changes to
consensus rules, transaction bytes, or stored state require an explicit chain
reset or migration plan before deployment.

## Current baseline

The following controls already exist and should be preserved:

- Coin inputs are resolved from live UTXOs. Transaction validation checks
  ownership, duplicate inputs, value conservation, and exact protocol burn.
  Miner fees become outputs owned by the block miner.
- Extension Program transfers conserve share value. Registration, mint, and burn
  update extension asset records under authorization and supply rules.
- Block execution uses staged state and checks the resulting state root before
  committing. Node reorganization uses a staged ledger.
- After block execution, rollback, and snapshot restoration, the ledger
  reconciles live coin UTXOs with mined minus burned supply and live asset
  shares with recorded asset supply.
- Kernel UTXO insertion rejects duplicate coin IDs; extension share insertion
  rejects duplicate share IDs. Zero-value coin outputs are rejected. Block size
  is bounded.
- Consensus hashes use named domains; transaction authorization binds the
  genesis hash and signed intent.

## Phase 1 — Bind transitions to the expected state [DONE]

**Status: implemented in the kernel.** Rollback-journal integrity is checked
against the state roots already committed in block headers.

1. Before applying a non-genesis block, recompute the active ledger state root
   and compare it with the canonical tip's state root. Keep the existing
   parent-link check.
2. Before rolling back a tip, compare current state root with that tip's root.
   Undo journals on staged state, then compare the result with the parent
   block's root. Rolling back genesis must yield the defined empty root.
3. Reject any mismatch without changing the active ledger. Reorganization
   continues to use these same application and rollback paths.
4. Prefer roots already committed in block headers. A root in every
   transaction journal would enlarge snapshots and repeat state hashing.

**Acceptance:** Tests detect tampered prestate, altered or incomplete journals,
wrong parent state, and failed reorganization. The canonical ledger and
persisted chain remain unchanged after each failure.

## Phase 2 — Prove failure atomicity [DONE]

**Status: implemented and verified for current transition paths.**

After legacy asset removal, coin mutation failure-injection tests remain active.
Program transactions stage coin payment and extension execution together;
block tests cover failed multi-call execution and corrupt rollback journals.

Inject controlled failures after input consumption, output creation, fee and
burn updates, asset register/mint/burn changes, emission creation, snapshot
loading, and reorganization branch application.

Compare complete state, root, tip, journals, and database-visible chain with
the pre-operation values. Audit helpers that mutate caller-owned state: on
error they must either roll back fully or remain inside discarded staged state.

**Acceptance:** Every injected failure leaves canonical state unchanged. A
successful retry produces the same state root as uninterrupted execution.

## Phase 3 — Bound consensus and decoding work [DONE]

**Status: implemented and tested for current transaction and block paths.**
Consensus caps each serialized transaction at 256 KiB, each
coin transaction input/output list at 4096 items, and a block at 4096
transactions. Borsh decoding rejects oversized list prefixes before reading
their elements. Block decoding also limits bytes read for each transaction,
and in-memory block validation counts serialized bytes with a capped writer
instead of allocating an unbounded block buffer.
Direct kernel validation checks the same limits before
signature verification. Node RPC, relay, and stored mempool transactions use
the transaction byte limit. These consensus changes are reflected in chain
spec version 1 for the reset baseline (coin-only kernel state and Program asset
transactions) and require fresh compatible chain/storage. Extension asset calls
limit transfer/burn inputs to 256 shares per call; transfer allows 256 outputs. See [ProgramCall integration](PROGRAM_CALL.md).

Malformed list prefixes, block replay bytes, oversized direct transactions,
RPC lengths, and P2P frame lengths have focused regression tests. In a local
run, these rejection tests each completed within 0.1 s and the test processes
peaked near 11 MiB RSS; these historical observations predate legacy removal
and are not measurements of the current Program path or consensus thresholds.

Audit transaction bytes, input and output counts, asset share counts, block
transaction counts, and vector lengths during decoding. Apply bounds before
large allocations or expensive signature verification. Consensus limits
define block validity; node relay limits may be stricter. Review validation
order so cheap structural and duplicate checks run early where this preserves
consensus behavior.

The existing 4 MiB block limit does not by itself bound every CPU or
allocation path.

**Acceptance:** Pathological payloads fail within measured memory and time
limits through RPC, P2P, block replay, and direct kernel validation.

## Phase 4 — Lock down deterministic execution [DONE]

Create permanent vectors for serialized transactions, authorization
commitments, blocks, expected UTXOs and supply counters, and resulting state
roots. Cover emission, asset operations, rollback, and divergent reorganization.
Run them after serialization, cryptography, monetary, or toolchain changes.

The current mainnet fixture covers XPQ emission/spend and signed Program
register, mint, transfer and burn blocks. Legacy asset vectors were replaced;
the chain-spec identity fixture now locks the reset baseline at version 1.

Add a block-level accounting check:

    live_coin_supply_after
      = live_coin_supply_before + validated_subsidy - all_protocol_burns

This complements per-spend conservation and the existing full UTXO-to-record
reconciliation; it does not replace either one.

**Acceptance:** Identical input state and block bytes always yield identical
state bytes and root. An unexpected supply delta rejects the block.

## Phase 5 — Audit existing protections

These are primarily audits and focused fixes rather than new protocol layers:

- **Integer safety:** Check monetary and state-weight operations for checked
  arithmetic; avoid saturating monetary consensus arithmetic.
- **Canonical encoding:** Verify one accepted byte representation for each
  signed or hashed consensus object, including rejection of trailing bytes.
- **Hash domains and authorization:** Confirm transaction type, genesis,
  inputs, outputs, fees, and asset operations are bound. Test cross-chain and
  cross-type replay.
- **Coin creation paths:** Keep spend outputs, block emission, and rollback
  restoration distinct; audit all UTXO insertion callers.
- **Startup consistency:** Validate tip linkage, snapshot root, supply
  reconciliation, and journal availability. Offer explicit full replay or
  verification if startup checks become too expensive.
- **Output policy:** Preserve zero-value rejection. Consider an economic
  minimum only after defining its consensus and wallet effects.
- **Reorg monitoring:** Alert on unusually deep reorganizations without
  silently adding a consensus finality rule.

**Acceptance:** Record each audit result, add focused regression tests for
defects found, and document compatibility decisions when consensus changes.

## Delivery gates

Complete implementation and focused tests for each phase before moving on.
Run workspace compilation, relevant kernel and node integration tests, format
checks, and git diff --check. Repair stale fixtures that block the relevant
gate; report unrelated failures separately. Verify replay and reorganization
on a fresh data directory before enabling consensus changes across a network.
