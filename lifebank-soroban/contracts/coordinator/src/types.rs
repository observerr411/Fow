use interfaces::ExcursionSummary;
use soroban_sdk::{contracttype, Vec};

// Cross-contract domain types (e.g. `ExcursionSummary`) are defined once in the
// shared `interfaces` crate and imported here — never hand-mirrored. The
// coordinator's cross-contract clients are likewise generated from the compiled
// interfaces (see `contractimport!` in `lib.rs`), so any interface change is
// caught by the compiler and the spec-drift CI check rather than at runtime.

/// Canonical workflow states — shared identifier across all contracts.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowStatus {
    /// Initial state before allocate_units is called.
    Pending,
    /// Units reserved, request approved.
    Allocated,
    /// All units delivered to hospital.
    Delivered,
    /// Payment released to blood bank.
    Settled,
    /// Workflow rolled back; units released, payment refunded.
    RolledBack,
    /// Workflow expired after allocation deadline; units released, payment refunded.
    Expired,
}

/// Per-request workflow record stored in the coordinator.
/// This is the canonical cross-contract state reference.
#[contracttype]
#[derive(Clone, Debug)]
pub struct WorkflowRecord {
    /// Stable identifier shared across all contracts.
    pub request_id: u64,
    /// Payment record ID in the payment contract.
    pub payment_id: u64,
    /// Inventory unit IDs allocated to this request.
    pub unit_ids: Vec<u64>,
    pub status: WorkflowStatus,
    pub delivery_confirmed: bool,
    /// ID of the inventory contract's `Reservation` record backing this
    /// allocation — the single source of truth for which units are held and
    /// until when (via `InventoryContractClient::get_reservation`).
    pub reservation_id: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    /// Guardian can pause instantly; only Admin can unpause.
    Guardian,
    RequestContract,
    InventoryContract,
    PaymentContract,
    IdentityContract,
    MatchingContract,
    Workflow(u64),
    /// Granular pause flags — stored as Option<u64> (paused_at timestamp).
    /// "Paused" is the legacy single bool kept for compat; new code uses PauseFlag.
    Paused,
    /// Per-operation pause flag. Value = ledger timestamp when paused.
    PauseFlag(soroban_sdk::Symbol),
}
