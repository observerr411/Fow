# lifebank-soroban

**The canonical Soroban contract stack for this repository.**
Per [ADR-0001](../docs/adr/README.md), this modular
workspace (soroban-sdk 23) is the single source of truth for on-chain logic. The
former `healthchain` monolith is deprecated and archived under
[`legacy/contracts/`](../legacy/contracts).

## Structure

```text
lifebank-soroban/
├── contracts/
│   ├── interfaces/     # shared event envelope, cross-contract types & clients
│   ├── coordinator/    # workflow orchestration, pause flags, timelocked upgrades
│   ├── inventory/      # blood unit registry & status lifecycle
│   ├── requests/       # blood request lifecycle
│   ├── matching/       # request ↔ inventory matching
│   ├── payments/       # escrow, disputes, pledges, vesting
│   ├── delivery/       # delivery proof & compliance attestation
│   ├── temperature/    # cold-chain telemetry & excursion reporting
│   ├── identity/       # organisation verification & attestation
│   ├── reputation/     # participant reputation
│   └── analytics/      # aggregate on-chain metrics
├── deploy/             # per-network deploy topology (*.toml)
├── deployments/        # per-network deployment lockfiles (contract IDs + WASM hashes)
├── docs/
│   ├── upgrades/       # contract upgrade runbook
│   └── migration/      # monolith → workspace port tracking (ADR-0001)
└── scripts/            # build, deploy, upgrade, smoke-test
```

Each contract has its own `Cargo.toml` and relies on the top-level workspace
`Cargo.toml` for shared dependencies.

## Shared interfaces

`contracts/interfaces/` is the **single source of truth** for every type that
crosses a contract boundary (`RequestStatus`, `BloodRequest`, `BloodStatus`,
`Payment`, …) and for the `contractclient` trait definitions of each domain
contract's public API. Both the implementing contract and every caller depend on
it, so a change to a shared type is compiler-enforced across the whole workspace
— no contract hand-mirrors another's types. The coordinator in particular
contains **zero** hand-duplicated domain types.

Where a contract only *calls* another, prefer `soroban_sdk::contractimport!` on
the built WASM so the client is generated from the actual compiled interface.
Because of this, `scripts/build-all.sh` builds in dependency order
(`interfaces` → domain contracts → `coordinator`).

### Append-only enums

`contracttype` enums used across contract boundaries are **append-only**:

- ✅ Adding a new variant at the end is a compatible change.
- ❌ Removing or reordering variants shifts enum discriminants and silently
  mis-decodes cross-contract return values at runtime (e.g. mis-settled
  payments). This requires an explicit **schema-version bump** (see #31) and a
  coordinated migration.

This rule is enforced by the spec-snapshot drift check below and by review.

### Spec-snapshot drift check

CI builds every WASM, extracts its contract spec (`stellar contract inspect` /
spec XDR) and diffs it against the committed snapshots under
`lifebank-soroban/specs/`. Any interface change fails CI until the snapshot is
updated in the same PR, making breaking changes visible at review time.

To update snapshots after an intentional interface change:

```bash
bash scripts/build-all.sh
bash scripts/update-spec-snapshots.sh   # regenerates specs/*.json
git add lifebank-soroban/specs && git commit
```

### TypeScript types for the backend

The NestJS backend (`backend/src/__contracts__`, `contract-event-indexer`)
consumes generated types rather than hand-written ones. As part of the build,
the contract spec XDR is translated to TypeScript so all three layers (contracts,
spec snapshots, backend) share one source of truth:

```bash
bash scripts/gen-ts-types.sh            # spec XDR → backend/src/__contracts__/generated
```

Do not edit the generated files by hand.

## Storage classes & TTL policy

Every persistent entry in `payments` and `inventory` has a documented storage
class and TTL policy. TTLs are bumped on access, and domain deadlines extend the
relevant entries (at `lock`, a payment's TTL is extended to at least the
dispute-resolution horizon; blood-unit TTLs are extended past product expiry).

| Contract  | Key / entry            | Class      | Threshold → extend-to | Notes |
|-----------|------------------------|------------|-----------------------|-------|
| payments  | `Payment` (escrow)     | persistent | per-class policy      | extended to dispute-resolution horizon at `lock` |
| payments  | pledges / vesting      | persistent | per-class policy      | bumped on access |
| inventory | blood-unit record      | persistent | per-class policy      | extended past product expiry |
| inventory | status/lifecycle index | persistent | per-class policy      | bumped on access |
| both      | instance storage       | instance   | extended at entrypoints | |

Reading an entry past the policy horizon yields a documented, distinct error
rather than a host panic. See each contract's source/README for the exact
constants (`LEDGERS_PER_DAY`, per-class thresholds).

## Build

```bash
cargo test --all                       # unit + property tests
cargo build --release --target wasm32v1-none
bash scripts/build-all.sh              # dependency-ordered build + wasm-opt
```

## Deploy

```bash
bash scripts/deploy-testnet.sh
```

Deployment writes a lockfile to `deployments/<network>.json`; CI
(`.github/workflows/deploy-verify.yml`) verifies built WASM hashes against it for
every network with a non-null `deployed_at`.

## Upgrades

All contracts upgrade in place (same contract ID, storage preserved). Payments
and coordinator require a 48h timelock. See [`docs/upgrades/`](docs/upgrades/).

## Migration from the monolith

The disposition of every monolith capability, invariant test, and fuzz target is
tracked in [`docs/migration/`](docs/migration/) against ADR-0001.
