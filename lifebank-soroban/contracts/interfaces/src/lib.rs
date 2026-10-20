#![no_std]
//! Shared interface crate for all lifebank-soroban contracts.
//!
//! This crate is the **single source of truth** for every type that crosses a
//! contract boundary. Both the implementing contract and every caller depend
//! on it, so a change to a shared type is compiler-enforced rather than
//! hand-mirrored (see issue #32).
//!
//! Layout:
//! - [`types`] — canonical `#[contracttype]` structs/enums shared across
//!   contracts (`RequestStatus`, `BloodRequest`, `BloodType`, `BloodStatus`,
//!   `BloodUnit`, `PaymentStatus`, `DisputeReason`, `Payment`, `CredentialType`,
//!   `Reservation`, `ExcursionSummary`, …).
//! - [`clients`] — `#[contractclient]` trait definitions for each domain
//!   contract's public API, so callers get a typed client generated from the
//!   same definition the implementer uses.
//! - [`envelope`] / [`events`] — the shared event envelope and versioned
//!   payload structs (see `lifebank-soroban/EVENTS.md`).
//! - [`ttl`] — shared storage TTL policy (storage classes, thresholds, and
//!   `bump` helpers) used by `payments` and `inventory` (see issue #52).
//!
//! ## Append-only enum policy
//!
//! `#[contracttype]` enums that are used across contract boundaries are
//! **append-only**: they may only gain new variants **at the end** of the
//! variant list. Soroban encodes enum variants by their ordinal discriminant,
//! so removing or reordering a variant silently changes the wire encoding and
//! causes callers to mis-decode values — a correctness bug that type-checks
//! fine and only surfaces at runtime.
//!
//! Removing or reordering a variant is a breaking change and requires a
//! schema-version bump (see #31). The CI spec-snapshot drift check (#32)
//! makes any such change visible at review time.
//!
//! ## Event envelope convention
//!
//! Every event published by every contract:
//! - topics = `(contract_domain: Symbol, event_name: Symbol, schema_version: u32)`
//! - data   = a `#[contracttype]` struct defined in `events::<contract_domain>`.
//!
//! Payload structs are append-only: add fields, never remove/rename/retype one
//! in place. A breaking change to an existing event gets a new struct and the
//! emitter bumps `schema_version` for that event name.

pub mod clients;
pub mod envelope;
pub mod events;
pub mod ttl;
pub mod types;

pub use types::*;
</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
</｜｜DSML｜｜ calls>