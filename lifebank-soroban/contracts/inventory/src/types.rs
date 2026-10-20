use crate::error::ContractError;
use interfaces::inventory::{BloodStatus, BloodType, BloodUnit, Reservation};
use soroban_sdk::{contracttype, Address, String, Vec};

// Cross-contract types (`BloodType`, `BloodStatus`, `BloodUnit`, `Reservation`)
// are defined once in the shared `interfaces` crate and re-exported here so the
// inventory contract and every caller share a single, compiler-enforced definition.
pub use interfaces::inventory::{BloodStatus as SharedBloodStatus, BloodType as SharedBloodType};

/// Single source of truth for legal `(from, to)` blood status transitions.
///
/// Every other check (`is_valid_transition`, tests) is derived from this list
/// so the matrix cannot drift across the codebase.
pub const ALLOWED_BLOOD_STATUS_TRANSITIONS: &[(BloodStatus, BloodStatus)] = {
    use BloodStatus::*;
    &[
        (Available, Reserved),
        (Available, Expired),
        (Available, Compromised),
        (Reserved, InTransit),
        (Reserved, Available),
        (Reserved, Expired),
        (Reserved, Compromised),
        (InTransit, Delivered),
        (InTransit, Expired),
        (InTransit, Compromised),
        (Expired, Disposed),
        (Compromised, Disposed),
    ]
};

/// Blood Unit Lifecycle Transition Map
///
/// The blood unit follows a strict forward-only lifecycle through the supply chain:
///
///   Available ──► Reserved ──► InTransit ──► Delivered (terminal)
///       │             │             │
///       ▼             ▼             ▼
///    Expired       Expired       Expired ──► Disposed (terminal)
///
///   Compromised ──► Disposed (terminal)
///   Reserved can also cancel back to Available.
///
/// Valid transitions: see [`ALLOWED_BLOOD_STATUS_TRANSITIONS`]. Operational states
/// may move to `Compromised` when temperature or chain-of-custody rules fail.
///
/// All other transitions are invalid and will be rejected. Backwards transitions
/// (e.g., Delivered → Available, InTransit → Reserved) are explicitly forbidden
/// to preserve the integrity of the on-chain audit trail.
///
/// This is a pure function with no storage access, making it unit-testable in isolation.
pub fn is_valid_transition(from: &BloodStatus, to: &BloodStatus) -> bool {
    ALLOWED_BLOOD_STATUS_TRANSITIONS
        .iter()
        .any(|(a, b)| a == from && b == to)
}

/// Validate that the blood unit data is consistent and valid.
///
/// Checks:
/// - Quantity is within acceptable range (100-600ml)
/// - Expiration is after donation
/// - Timestamps are reasonable (not in far future)
pub fn validate_blood_unit(unit: &BloodUnit, current_time: u64) -> Result<(), ContractError> {
    // Validate quantity (typical range: 100-600ml)
    if unit.quantity_ml < 100 || unit.quantity_ml > 600 {
        return Err(ContractError::InvalidQuantity);
    }

    // Validate timestamps
    if unit.expiration_timestamp <= unit.donation_timestamp {
        return Err(ContractError::InvalidTimestamp);
    }

    // Donation shouldn't be from far future (allow up to 1 hour ahead for clock skew)
    if unit.donation_timestamp > current_time + 3600 {
        return Err(ContractError::InvalidTimestamp);
    }

    Ok(())
}

/// Check if blood unit is currently expired
pub fn is_expired(unit: &BloodUnit, current_time: u64) -> bool {
    current_time >= unit.expiration_timestamp
}

/// Calculate shelf life remaining in seconds
pub fn shelf_life_remaining(unit: &BloodUnit, current_time: u64) -> i64 {
    (unit.expiration_timestamp as i64) - (current_time as i64)
}

/// Storage key types for efficient querying
#[contracttype]
#[derive(Clone, Debug)]
pub enum DataKey {
    /// Individual blood unit by ID
    BloodUnit(u64),

    /// Counter for generating new blood unit IDs
    BloodUnitCounter,

    /// Index: Blood type -> Vec<u64> (blood unit IDs)
    BloodTypeIndex(BloodType),

    /// Index: Bank ID -> Vec<u64> (blood unit IDs)
    BankIndex(Address),

    /// Index: Status -> Vec<u64> (blood unit IDs)
    StatusIndex(BloodStatus),

    /// Index: Donor ID -> Vec<u64> (blood unit IDs)
    DonorIndex(Address),

    /// Admin address
    Admin,

    /// Status change history for a blood unit — stores current page number
    StatusHistory(u64),

    /// One page of status change history: (blood_unit_id, page_number)
    StatusHistoryPage(u64, u32),

    /// Counter for status change history records
    StatusHistoryCounter,

    /// Counter for status changes on specific blood unit
    BloodUnitStatusChangeCount(u64), // u64 is blood_unit_id

    /// Reservation record by reservation ID
    Reservation(u64),

    /// Reservation counter
    ReservationCounter,

    /// Circuit breaker: contract is paused
    Paused,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct BloodRegisteredEvent {
    /// Unique ID of the registered blood unit
    pub blood_unit_id: u64,

    /// Blood bank that registered the unit
    pub bank_id: Address,

    /// Blood type
    pub blood_type: BloodType,

    /// Quantity in milliliters
    pub quantity_ml: u32,

    /// When the unit expires
    pub expiration_timestamp: u64,

    /// When the unit was registered
    pub registered_at: u64,
}

/// Event emitted when blood unit status changes
#[contracttype]
#[derive(Clone, Debug)]
pub struct StatusChangeEvent {
    /// Unique ID of the blood unit
    pub blood_unit_id: u64,

    /// Previous status
    pub from_status: BloodStatus,

    /// New status
    pub to_status: BloodStatus,

    /// Who authorized this change
    pub authorized_by: Address,

    /// When the status change occurred
    pub changed_at: u64,

    /// Optional reason for status change (e.g., "Delivered to Hospital A")
    pub reason: Option<String>,
}

/// On-chain audit event for every blood unit status transition.
/// Emitted as `blood_unit_status_changed` — immutable once published.
#[contracttype]
#[derive(Clone, Debug)]
pub struct AuditEvent {
    /// Blood unit that transitioned
    pub unit_id: u64,
    /// Status before the transition
    pub previous_status: BloodStatus,
    /// Status after the transition
    pub new_status: BloodStatus,
    /// Address that authorised the transition
    pub actor: Address,
    /// Ledger timestamp of the transition
    pub timestamp: u64,
}

/// Historical record of a status change
#[contracttype]
#[derive(Clone, Debug)]
pub struct StatusChangeHistory {
    /// Unique ID for this history record
    pub id: u64,

    /// Blood unit ID
    pub blood_unit_id: u64,

    /// Previous status
    pub from_status: BloodStatus,

    /// New status
    pub to_status: BloodStatus,

    /// Who authorized this change
    pub authorized_by: Address,

    /// When the status change occurred
    pub changed_at: u64,

    /// Optional reason for status change
    pub reason: Option<String>,
}

/// Batch status update operation
#[contracttype]
#[derive(Clone, Debug)]
pub struct BatchStatusUpdate {
    /// List of blood unit IDs to update
    pub blood_unit_ids: Vec<u64>,

    /// New status for all units
    pub new_status: BloodStatus,

    /// Optional reason for batch update
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn test_blood_type_compatibility_universal_donor() {
        let o_neg = BloodType::ONegative;

        // O- can donate to all types
        assert!(o_neg.can_donate_to(&BloodType::ONegative));
        assert!(o_neg.can_donate_to(&BloodType::OPositive));
        assert!(o_neg.can_donate_to(&BloodType::ANegative));
        assert!(o_neg.can_donate_to(&BloodType::APositive));
        assert!(o_neg.can_donate_to(&BloodType::BNegative));
        assert!(o_neg.can_donate_to(&BloodType::BPositive));
        assert!(o_neg.can_donate_to(&BloodType::ABNegative));
        assert!(o_neg.can_donate_to(&BloodType::ABPositive));
    }

    #[test]
    fn test_blood_type_compatibility_universal_recipient() {
        let ab_pos = BloodType::ABPositive;

        // AB+ can receive from all types
        assert!(BloodType::ONegative.can_donate_to(&ab_pos));
        assert!(BloodType::OPositive.can_donate_to(&ab_pos));
        assert!(BloodType::ANegative.can_donate_to(&ab_pos));
        assert!(BloodType::APositive.can_donate_to(&ab_pos));
        assert!(BloodType::BNegative.can_donate_to(&ab_pos));
        assert!(BloodType::BPositive.can_donate_to(&ab_pos));
        assert!(BloodType::ABNegative.can_donate_to(&ab_pos));
        assert!(BloodType::ABPositive.can_donate_to(&ab_pos));
    }

    #[test]
    fn test_blood_type_compatibility_specific_cases() {
        // A+ can donate to A+ and AB+
        assert!(BloodType::APositive.can_donate_to(&BloodType::APositive));
        assert!(BloodType::APositive.can_donate_to(&BloodType::ABPositive));
        assert!(!BloodType::APositive.can_donate_to(&BloodType::ANegative));
        assert!(!BloodType::APositive.can_donate_to(&BloodType::BPositive));

        // B- can donate to B+, B-, AB+, AB-
        assert!(BloodType::BNegative.can_donate_to(&BloodType::BPositive));
        assert!(BloodType::BNegative.can_donate_to(&BloodType::BNegative));
        assert!(BloodType::BNegative.can_donate_to(&BloodType::ABPositive));
        assert!(BloodType::BNegative.can_donate_to(&BloodType::ABNegative));
        assert!(!BloodType::BNegative.can_donate_to(&BloodType::APositive));
    }

    #[test]
    fn test_allowed_transitions_match_is_valid_transition() {
        use super::{is_valid_transition, ALLOWED_BLOOD_STATUS_TRANSITIONS};
        for (from, to) in ALLOWED_BLOOD_STATUS_TRANSITIONS {
            assert!(
                is_valid_transition(from, to),
                "allowlist pair ({:?}, {:?}) must be accepted",
                from,
                to
            );
        }
    }

    /// Property-style: every pair of statuses is valid iff it appears in the allowlist.
    #[test]
    fn test_transition_matrix_exhaustive_against_allowlist() {
        use super::{is_valid_transition, ALLOWED_BLOOD_STATUS_TRANSITIONS, BloodStatus};
        let mut allowed_set = [false; 7 * 7];
        let idx = |s: BloodStatus| match s {
            BloodStatus::Available => 0,
            BloodStatus::Reserved => 1,
            BloodStatus::InTransit => 2,
            BloodStatus::Delivered => 3,
            BloodStatus::Expired => 4,
            BloodStatus::Compromised => 5,
            BloodStatus::Disposed => 6,
        };
        for (from, to) in ALLOWED_BLOOD_STATUS_TRANSITIONS {
            allowed_set[idx(*from) * 7 + idx(*to)] = true;
        }
        for from in BloodStatus::ALL {
            for to in BloodStatus::ALL {
                let expect = allowed_set[idx(from) * 7 + idx(to)];
                assert_eq!(
                    is_valid_transition(&from, &to),
                    expect,
                    "pair ({:?}, {:?}) mismatch",
                    from,
                    to
                );
            }
        }
    }

    #[test]
    fn test_is_valid_transition_invalid_backwards() {
        use super::is_valid_transition;
        use BloodStatus::*;

        // 1. Delivered -> Available (backwards)
        assert!(!is_valid_transition(&Delivered, &Available));
        // 2. Delivered -> Reserved (backwards)
        assert!(!is_valid_transition(&Delivered, &Reserved));
        // 3. Delivered -> InTransit (backwards)
        assert!(!is_valid_transition(&Delivered, &InTransit));
        // 4. Delivered -> Expired (terminal cannot transition)
        assert!(!is_valid_transition(&Delivered, &Expired));
        // 5. Expired -> Available (backwards from terminal)
        assert!(!is_valid_transition(&Expired, &Available));
        // 6. Expired -> Reserved (backwards from terminal)
        assert!(!is_valid_transition(&Expired, &Reserved));
        // 7. Expired -> InTransit (backwards from terminal)
        assert!(!is_valid_transition(&Expired, &InTransit));
        // 8. Expired -> Delivered (backwards from terminal)
        assert!(!is_valid_transition(&Expired, &Delivered));
        // 9. InTransit -> Available (skip backwards)
        assert!(!is_valid_transition(&InTransit, &Available));
        // 10. InTransit -> Reserved (backwards)
        assert!(!is_valid_transition(&InTransit, &Reserved));
        // 11. Available -> Delivered (skip forward)
        assert!(!is_valid_transition(&Available, &Delivered));
        // 12. Available -> InTransit (skip forward)
        assert!(!is_valid_transition(&Available, &InTransit));
        // 13. Reserved -> Delivered (skip forward — must go through InTransit)
        assert!(!is_valid_transition(&Reserved, &Delivered));
        // 14. Delivered -> Disposed
        assert!(!is_valid_transition(&Delivered, &Disposed));
        // 15. Compromised -> Available (illegal recovery)
        assert!(!is_valid_transition(&Compromised, &Available));
    }

    #[test]
    fn test_is_valid_transition_self_transitions_invalid() {
        use super::is_valid_transition;
        use BloodStatus::*;

        // No status should be able to transition to itself
        assert!(!is_valid_transition(&Available, &Available));
        assert!(!is_valid_transition(&Reserved, &Reserved));
        assert!(!is_valid_transition(&InTransit, &InTransit));
        assert!(!is_valid_transition(&Delivered, &Delivered));
        assert!(!is_valid_transition(&Expired, &Expired));
        assert!(!is_valid_transition(&Compromised, &Compromised));
        assert!(!is_valid_transition(&Disposed, &Disposed));
    }

    #[test]
    fn test_status_terminal_states() {
        assert!(BloodStatus::Delivered.is_terminal());
        assert!(BloodStatus::Disposed.is_terminal());
        assert!(!BloodStatus::Compromised.is_terminal());
        assert!(!BloodStatus::Expired.is_terminal());
        assert!(!BloodStatus::Available.is_terminal());
        assert!(!BloodStatus::Reserved.is_terminal());
        assert!(!BloodStatus::InTransit.is_terminal());
    }

    #[test]
    fn test_blood_unit_validation_valid() {
        let env = Env::default();
        let bank = Address::generate(&env);
        let current_time = 1000u64;

        let unit = BloodUnit {
            id: 1,
            blood_type: BloodType::APositive,
            quantity_ml: 450,
            bank_id: bank,
            donor_id: None,
            donation_timestamp: current_time,
            expiration_timestamp: current_time + (42 * 24 * 60 * 60), // 42 days
            status: BloodStatus::Available,
            metadata: Map::new(&env),
        };

        assert!(validate_blood_unit(&unit, current_time).is_ok());
    }

    #[test]
    fn test_blood_unit_validation_invalid_quantity_too_low() {
        let env = Env::default();
        let bank = Address::generate(&env);
        let current_time = 1000u64;

        let unit = BloodUnit {
            id: 1,
            blood_type: BloodType::APositive,
            quantity_ml: 50, // Too low
            bank_id: bank,
            donor_id: None,
            donation_timestamp: current_time,
            expiration_timestamp: current_time + (42 * 24 * 60 * 60),
            status: BloodStatus::Available,
            metadata: Map::new(&env),
        };

        assert_eq!(
            validate_blood_unit(&unit, current_time),
            Err(ContractError::InvalidQuantity)
        );
    }

    #[test]
    fn test_blood_unit_validation_invalid_quantity_too_high() {
        let env = Env::default();
        let bank = Address::generate(&env);
        let current_time = 1000u64;

        let unit = BloodUnit {
            id: 1,
            blood_type: BloodType::APositive,
            quantity_ml: 700, // Too high
            bank_id: bank,
            donor_id: None,
            donation_timestamp: current_time,
            expiration_timestamp: current_time + (42 * 24 * 60 * 60),
            status: BloodStatus::Available,
            metadata: Map::new(&env),
        };

        assert_eq!(
            validate_blood_unit(&unit, current_time),
            Err(ContractError::InvalidQuantity)
        );
    }

    #[test]
    fn test_blood_unit_validation_expiration_before_donation() {
        let env = Env::default();
        let bank = Address::generate(&env);
        let current_time = 1000u64;

        let unit = BloodUnit {
            id: 1,
            blood_type: BloodType::APositive,
            quantity_ml: 450,
            bank_id: bank,
            donor_id: None,
            donation_timestamp: current_time,
            expiration_timestamp: current_time - 100, // Before donation
            status: BloodStatus::Available,
            metadata: Map::new(&env),
        };

        assert_eq!(
            validate_blood_unit(&unit, current_time),
            Err(ContractError::InvalidTimestamp)
        );
    }

    #[test]
    fn test_blood_unit_validation_future_donation() {
        let env = Env::default();
        let bank = Address::generate(&env);
        let current_time = 1000u64;

        let unit = BloodUnit {
            id: 1,
            blood_type: BloodType::APositive,
            quantity_ml: 450,
            bank_id: bank,
            donor_id: None,
            donation_timestamp: current_time + 7200, // 2 hours in future
            expiration_timestamp: current_time + (42 * 24 * 60 * 60),
            status: BloodStatus::Available,
            metadata: Map::new(&env),
        };

        assert_eq!(
            validate_blood_unit(&unit, current_time),
            Err(ContractError::InvalidTimestamp)
        );
    }

    #[test]
    fn test_blood_unit_is_expired() {
        let env = Env::default();
        let bank = Address::generate(&env);
        let donation_time = 1000u64;
        let expiration_time = donation_time + (42 * 24 * 60 * 60);

        let unit = BloodUnit {
            id: 1,
            blood_type: BloodType::APositive,
            quantity_ml: 450,
            bank_id: bank,
            donor_id: None,
            donation_timestamp: donation_time,
            expiration_timestamp: expiration_time,
            status: BloodStatus::Available,
            metadata: Map::new(&env),
        };

        // Not expired before expiration time
        assert!(!is_expired(&unit, expiration_time - 1));

        // Expired at expiration time
        assert!(is_expired(&unit, expiration_time));

        // Expired after expiration time
        assert!(is_expired(&unit, expiration_time + 100));
    }

    #[test]
    fn test_blood_unit_shelf_life_remaining() {
        let env = Env::default();
        let bank = Address::generate(&env);
        let donation_time = 1000u64;
        let expiration_time = donation_time + 3600; // 1 hour

        let unit = BloodUnit {
            id: 1,
            blood_type: BloodType::APositive,
            quantity_ml: 450,
            bank_id: bank,
            donor_id: None,
            donation_timestamp: donation_time,
            expiration_timestamp: expiration_time,
            status: BloodStatus::Available,
            metadata: Map::new(&env),
        };

        // 30 minutes before expiration
        assert_eq!(shelf_life_remaining(&unit, donation_time + 1800), 1800);

        // At expiration
        assert_eq!(shelf_life_remaining(&unit, expiration_time), 0);

        // 10 minutes past expiration
        assert_eq!(shelf_life_remaining(&unit, expiration_time + 600), -600);
    }
}
