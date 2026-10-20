#![no_std]

/// Cross-contract coordinator for the HealthDonor workflow.
///
/// Canonical workflow sequence enforced here:
///   1. allocate_units  – Request must be Pending; reserves inventory units
///   2. confirm_delivery – Workflow must be Allocated; marks units Delivered
///   3. settle_payment   – Workflow must be Delivered; releases escrowed payment
///
/// Any step that finds the prerequisite state missing returns an error and makes
/// no state changes, providing safe rollback semantics within a single transaction.
mod error;
mod events;
mod types;

#[cfg(test)]
mod test;

pub use error::CoordinatorError;
pub use types::{DataKey, ExcursionSummary, WorkflowRecord, WorkflowStatus};

use soroban_sdk::{contract, contractimpl, contracttype, symbol_short, Address, Env, String, Vec};

// All cross-contract types and clients come from the shared `interfaces`
// crate — the single, compiler-enforced definition shared with every domain
// contract. The coordinator holds no hand-mirrored domain types.
use interfaces::{
    BloodRequest, BloodStatus, BloodType, BloodUnit, CredentialType, DisputeReason,
    IdentityContractClient, InventoryContractClient, Payment, PaymentContractClient,
    PaymentStatus, RequestContractClient, RequestStatus, Reservation,
};

/// Maximum duration any pause flag may remain active (30 days).
/// After this window all workflow paths auto-enable even if admin keys are lost.
pub const MAX_PAUSE_SECS: u64 = 30 * 24 * 3600;

/// Returns true if `donor` blood type is compatible with `recipient`.
/// Mirrors matching::is_compatible — kept inline to avoid a cross-contract
/// read just for a pure predicate.
fn is_compatible(donor: BloodType, recipient: BloodType) -> bool {
    if donor == BloodType::ONegative { return true; }
    if recipient == BloodType::ABPositive { return true; }
    use BloodType::*;
    matches!(
        (donor, recipient),
        (OPositive,  OPositive | APositive | BPositive | ABPositive)
        | (ANegative, ANegative | APositive | ABNegative | ABPositive)
        | (APositive, APositive | ABPositive)
        | (BNegative, BNegative | BPositive | ABNegative | ABPositive)
        | (BPositive, BPositive | ABPositive)
        | (ABNegative, ABNegative | ABPositive)
        | (ABPositive, ABPositive)
    )
}

const ALLOCATION_EXPIRY_SECONDS: u64 = 24 * 60 * 60;

// ── Storage helpers ────────────────────────────────────────────────────────────

fn get_admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn load_workflow(env: &Env, request_id: u64) -> Option<WorkflowRecord> {
    env.storage()
        .persistent()
        .get(&DataKey::Workflow(request_id))
}

fn save_workflow(env: &Env, wf: &WorkflowRecord) {
    env.storage()
        .persistent()
        .set(&DataKey::Workflow(wf.request_id), wf);
}

// ── Contract ───────────────────────────────────────────────────────────────────

#[contract]
pub struct CoordinatorContract;

#[contractimpl]
impl CoordinatorContract {
    /// Atomic constructor — deploy + init in a single transaction.
    ///
    /// soroban-sdk 22+ executes `__constructor` as part of the deploy call,
    /// eliminating the window between deploy and initialize during which any
    /// caller could front-run and claim admin by calling `initialize` first.
    ///
    /// The separate `initialize` entry-point is kept with an already-initialized
    /// guard for tooling compatibility, but is a no-op if the constructor ran.
    #[allow(clippy::too_many_arguments)]
    pub fn __constructor(
        env: Env,
        admin: Address,
        request_contract: Address,
        inventory_contract: Address,
        payment_contract: Address,
    ) {
        admin.require_auth();
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        // Assert that the inventory contract's admin is this coordinator contract,
        // so update_status calls authorised by inv_admin are actually authorised
        // by the coordinator itself — not an implicit side-effect of address config.
        let coord_addr = env.current_contract_address();
        let inv_admin = InventoryContractClient::new(&env, &inventory_contract).get_admin();
        if inv_admin != coord_addr {
            panic!("inventory admin must be set to this coordinator contract address");
        }
        let pay_coordinator = PaymentContractClient::new(&env, &payment_contract).get_coordinator();
        if pay_coordinator != Some(coord_addr.clone()) {
            panic!("payments coordinator must be set to this coordinator contract address");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::RequestContract, &request_contract);
        env.storage()
            .instance()
            .set(&DataKey::InventoryContract, &inventory_contract);
        env.storage()
            .instance()
            .set(&DataKey::PaymentContract, &payment_contract);
        env.storage()
            .instance()
            .set(&SCHEMA_VERSION_KEY, &TARGET_SCHEMA_VERSION);
        env.events().publish(
            (
                symbol_short!("coord"),
                symbol_short!("init"),
                symbol_short!("v1"),
            ),
            admin,
        );
    }

    /// Legacy initialize — kept for tooling compatibility.
    /// Returns `AlreadyInitialized` if the constructor already ran (the
    /// normal case); otherwise performs first-time initialization so the
    /// contract can be used without a constructor-aware deployer.
    pub fn initialize(
        env: Env,
        admin: Address,
        request_contract: Address,
        inventory_contract: Address,
        payment_contract: Address,
    ) -> Result<(), CoordinatorError> {
        admin.require_auth();
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(CoordinatorError::AlreadyInitialized);
        }
        // Same admin-wiring assertion as the constructor.
        let coord_addr = env.current_contract_address();
        let inv_admin = InventoryContractClient::new(&env, &inventory_contract).get_admin();
        if inv_admin != coord_addr {
            return Err(CoordinatorError::InventoryAdminMismatch);
        }
        let pay_coordinator = PaymentContractClient::new(&env, &payment_contract).get_coordinator();
        if pay_coordinator != Some(coord_addr.clone()) {
            return Err(CoordinatorError::PaymentCoordinatorMismatch);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::RequestContract, &request_contract);
        env.storage()
            .instance()
            .set(&DataKey::InventoryContract, &inventory_contract);
        env.storage()
            .instance()
            .set(&DataKey::PaymentContract, &payment_contract);
        env.events().publish(
            (
                symbol_short!("coord"),
                symbol_short!("init"),
                symbol_short!("v1"),
            ),
            admin,
        );
        Ok(())
    }

    /// Set the guardian address. Admin only.
    /// Guardian can pause any flag instantly; only Admin can unpause.
    pub fn set_guardian(
        env: Env,
        admin: Address,
        guardian: Address,
    ) -> Result<(), CoordinatorError> {
        admin.require_auth();
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(CoordinatorError::Unauthorized)?;
        if admin != stored {
            return Err(CoordinatorError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Guardian, &guardian);
        env.events().publish(
            (symbol_short!("coord"), symbol_short!("guardian")),
            guardian,
        );
        Ok(())
    }

    /// Pause a specific operation flag. Guardian or Admin only.
    /// `flag`: symbol_short!("alloc") | symbol_short!("dlvr") | symbol_short!("settl").
    /// Pausing "alloc" does NOT block refund/rollback — exit paths remain open.
    pub fn pause_flag(
        env: Env,
        caller: Address,
        flag: soroban_sdk::Symbol,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(CoordinatorError::Unauthorized)?;
        let guardian: Option<Address> = env.storage().instance().get(&DataKey::Guardian);
        if caller != admin && guardian.as_ref() != Some(&caller) {
            return Err(CoordinatorError::Unauthorized);
        }
        Self::validate_flag(&flag)?;
        let now = env.ledger().timestamp();
        env.storage()
            .instance()
            .set(&DataKey::PauseFlag(flag.clone()), &now);
        env.events().publish(
            (symbol_short!("coord"), symbol_short!("paused")),
            (flag, now),
        );
        Ok(())
    }

    /// Unpause a specific operation flag. Admin only.
    /// Unpausing "settl" (settlement/release) requires the timelock to have elapsed.
    pub fn unpause_flag(
        env: Env,
        admin: Address,
        flag: soroban_sdk::Symbol,
    ) -> Result<(), CoordinatorError> {
        admin.require_auth();
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(CoordinatorError::Unauthorized)?;
        if admin != stored {
            return Err(CoordinatorError::Unauthorized);
        }
        Self::validate_flag(&flag)?;
        // Settlement flag is funds-critical: require timelock before unpausing.
        if flag == symbol_short!("settl") {
            if let Some(paused_at) = env
                .storage()
                .instance()
                .get::<_, u64>(&DataKey::PauseFlag(flag.clone()))
            {
                let now = env.ledger().timestamp();
                if now < paused_at + UPGRADE_TIMELOCK_SECS {
                    return Err(CoordinatorError::TimelockNotElapsed);
                }
            }
        }
        env.storage()
            .instance()
            .remove(&DataKey::PauseFlag(flag.clone()));
        env.events()
            .publish((symbol_short!("coord"), symbol_short!("unpaused")), flag);
        Ok(())
    }

    /// Returns whether a specific flag is currently paused (within safety window).
    pub fn is_flag_paused(env: Env, flag: soroban_sdk::Symbol) -> bool {
        Self::flag_is_paused_now(&env, &flag)
    }

    fn validate_flag(flag: &soroban_sdk::Symbol) -> Result<(), CoordinatorError> {
        if *flag == symbol_short!("alloc")
            || *flag == symbol_short!("dlvr")
            || *flag == symbol_short!("settl")
        {
            Ok(())
        } else {
            Err(CoordinatorError::UnknownPauseFlag)
        }
    }

    fn flag_is_paused_now(env: &Env, flag: &soroban_sdk::Symbol) -> bool {
        match env
            .storage()
            .instance()
            .get::<_, u64>(&DataKey::PauseFlag(flag.clone()))
        {
            None => false,
            Some(paused_at) => {
                let now = env.ledger().timestamp();
                now < paused_at + MAX_PAUSE_SECS
            }
        }
    }

    /// Legacy single-flag pause — pauses the "alloc" flag only.
    /// Rollback and expire remain callable. Admin or Guardian only.
    pub fn pause(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        admin.require_auth();
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(CoordinatorError::Unauthorized)?;
        let guardian: Option<Address> = env.storage().instance().get(&DataKey::Guardian);
        if admin != stored && guardian.as_ref() != Some(&admin) {
            return Err(CoordinatorError::Unauthorized);
        }
        let now = env.ledger().timestamp();
        env.storage()
            .instance()
            .set(&DataKey::PauseFlag(symbol_short!("alloc")), &now);
        env.events().publish(
            (symbol_short!("coord"), symbol_short!("paused")),
            (symbol_short!("alloc"), now),
        );
        Ok(())
    }

    /// Legacy single-flag unpause — unpauses the "alloc" flag. Admin only.
    pub fn unpause(env: Env, admin: Address) -> Result<(), CoordinatorError> {
        admin.require_auth();
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(CoordinatorError::Unauthorized)?;
        if admin != stored {
            return Err(CoordinatorError::Unauthorized);
        }
        env.storage()
            .instance()
            .remove(&DataKey::PauseFlag(symbol_short!("alloc")));
        env.events().publish(
            (symbol_short!("coord"), symbol_short!("unpaused")),
            symbol_short!("alloc"),
        );
        Ok(())
    }

    /// Returns true if the "alloc" flag is paused (legacy compat).
    pub fn is_paused(env: Env) -> bool {
        Self::flag_is_paused_now(&env, &symbol_short!("alloc"))
    }

    fn require_not_paused(env: &Env) -> Result<(), CoordinatorError> {
        if Self::flag_is_paused_now(env, &symbol_short!("alloc")) {
            return Err(CoordinatorError::ContractPaused);
        }
        Ok(())
    }

    /// Step 1 – Allocate inventory units to a pending request.
    pub fn allocate_units(
        env: Env,
        request_id: u64,
        unit_ids: Vec<u64>,
        payment_id: u64,
        caller: Address,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();
        Self::require_initialized(&env)?;
        if Self::flag_is_paused_now(&env, &symbol_short!("alloc")) {
            return Err(CoordinatorError::ContractPaused);
        }
        Self::require_compatible_domain_contracts(&env)?;

        if load_workflow(&env, request_id).is_some() {
            return Err(CoordinatorError::AlreadyDone);
        }

        // Re-check facility credential at allocation time (may have expired since request).
        // Uses caller as the facility subject — caller must hold MedicalFacilityLicense.
        if let Some(identity_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::IdentityContract)
        {
            let id_client = IdentityContractClient::new(&env, &identity_addr);
            let credentialed = id_client
                .try_is_valid(&caller, &CredentialType::MedicalFacilityLicense, &false)
                .unwrap_or(Ok(false))
                .unwrap_or(false);
            if !credentialed {
                return Err(CoordinatorError::Unauthorized);
            }
        }

        // Verify request is Pending
        let req_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::RequestContract)
            .unwrap();
        let req_client = RequestContractClient::new(&env, &req_addr);
        let request = req_client
            .try_get_request(&request_id)
            .map_err(|_| CoordinatorError::RequestNotFound)?
            .map_err(|_| CoordinatorError::RequestNotFound)?;

        if request.status != RequestStatus::Pending {
            return Err(CoordinatorError::InvalidRequestState);
        }

        // Validate every unit is Available and compatible with the request's
        // blood type before reserving anything — reserve_blood itself has no
        // notion of blood type, so this check must stay here.
        let inv_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::InventoryContract)
            .unwrap();
        let inv_client = InventoryContractClient::new(&env, &inv_addr);

        for i in 0..unit_ids.len() {
            let uid = unit_ids.get(i).unwrap();
            let unit = inv_client
                .try_get_blood_unit(&uid)
                .map_err(|_| CoordinatorError::UnitNotFound)?
                .map_err(|_| CoordinatorError::UnitNotFound)?;

            if unit.status != BloodStatus::Available {
                return Err(CoordinatorError::UnitNotAvailable);
            }

            if !is_compatible(unit.blood_type, request.blood_type) {
                return Err(CoordinatorError::IncompatibleBloodType);
            }
        }

        // Reserve the whole batch atomically through inventory's Reservation
        // record — the single source of truth for "who holds these units and
        // until when." The coordinator's own contract address is both the
        // inventory admin (asserted at init) and, by that same equality, an
        // authorized requester, so this self-authorizes exactly like the
        // inv_admin passthrough used elsewhere in this contract.
        let coord_addr = env.current_contract_address();
        let reservation_id = inv_client
            .try_reserve_blood(
                &coord_addr,
                &unit_ids,
                &request_id,
                &ALLOCATION_EXPIRY_SECONDS,
            )
            .map_err(|_| CoordinatorError::InventoryUpdateFailed)?
            .map_err(|_| CoordinatorError::InventoryUpdateFailed)?;

        env.events().publish(
            (
                symbol_short!("coord"),
                symbol_short!("alloc"),
                symbol_short!("v1"),
            ),
            (request_id, unit_ids.len()),
        );

        save_workflow(
            &env,
            &WorkflowRecord {
                request_id,
                payment_id,
                unit_ids,
                status: WorkflowStatus::Allocated,
                delivery_confirmed: false,
                reservation_id,
            },
        );

        Ok(())
    }

    /// Step 2 – Confirm delivery: mark all reserved units as Delivered.
    pub fn confirm_delivery(
        env: Env,
        request_id: u64,
        caller: Address,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();
        Self::require_initialized(&env)?;
        if Self::flag_is_paused_now(&env, &symbol_short!("dlvr")) {
            return Err(CoordinatorError::ContractPaused);
        }
        Self::require_compatible_domain_contracts(&env)?;

        let mut wf = load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;

        if wf.status == WorkflowStatus::Delivered || wf.status == WorkflowStatus::Settled {
            return Err(CoordinatorError::AlreadyDone);
        }

        if wf.status != WorkflowStatus::Allocated {
            return Err(CoordinatorError::InvalidWorkflowState);
        }

        let inv_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::InventoryContract)
            .unwrap();
        let inv_client = InventoryContractClient::new(&env, &inv_addr);
        let inv_admin = inv_client.get_admin();

        // Extend the reservation deadline by the pause duration so users are
        // not penalised — the reservation record in inventory is the sole
        // place this deadline lives, so extend it there rather than tracking
        // a second, competing deadline in WorkflowRecord.
        if let Some(paused_at) = env
            .storage()
            .instance()
            .get::<_, u64>(&DataKey::PauseFlag(symbol_short!("dlvr")))
        {
            let pause_duration = env.ledger().timestamp().saturating_sub(paused_at);
            inv_client
                .try_extend_reservation(&wf.reservation_id, &pause_duration, &inv_admin)
                .map_err(|_| CoordinatorError::InventoryUpdateFailed)?
                .map_err(|_| CoordinatorError::InventoryUpdateFailed)?;
        }

        let location = soroban_sdk::String::from_str(&env, "delivered");

        for i in 0..wf.unit_ids.len() {
            let uid = wf.unit_ids.get(i).unwrap();
            // Inventory enforces Reserved → InTransit → Delivered; coordinator must not skip InTransit.
            inv_client
                .try_update_status(&uid, &BloodStatus::InTransit, &inv_admin, &None)
                .map_err(|_| CoordinatorError::InventoryUpdateFailed)?
                .map_err(|_| CoordinatorError::InventoryUpdateFailed)?;
            inv_client
                .try_mark_delivered(&uid, &inv_admin, &location)
                .map_err(|_| CoordinatorError::InventoryUpdateFailed)?
                .map_err(|_| CoordinatorError::InventoryUpdateFailed)?;
        }

        wf.status = WorkflowStatus::Delivered;
        wf.delivery_confirmed = true;
        save_workflow(&env, &wf);

        env.events().publish(
            (
                symbol_short!("coord"),
                symbol_short!("dlvrd"),
                symbol_short!("v1"),
            ),
            request_id,
        );

        Ok(())
    }

    /// Step 3 – Settle payment. Blocked if delivery not confirmed.
    pub fn settle_payment(
        env: Env,
        request_id: u64,
        caller: Address,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();
        Self::require_initialized(&env)?;
        if Self::flag_is_paused_now(&env, &symbol_short!("settl")) {
            return Err(CoordinatorError::ContractPaused);
        }
        Self::require_compatible_domain_contracts(&env)?;

        let mut wf = load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;

        if wf.status == WorkflowStatus::Settled {
            return Err(CoordinatorError::AlreadyDone);
        }

        if !wf.delivery_confirmed || wf.status != WorkflowStatus::Delivered {
            return Err(CoordinatorError::DeliveryNotConfirmed);
        }

        let pay_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::PaymentContract)
            .unwrap();
        let pay_client = PaymentContractClient::new(&env, &pay_addr);

        let payment = pay_client
            .try_get_payment(&wf.payment_id)
            .map_err(|_| CoordinatorError::PaymentNotFound)?
            .map_err(|_| CoordinatorError::PaymentNotFound)?;

        if payment.status != PaymentStatus::Locked {
            return Err(CoordinatorError::InvalidPaymentState);
        }

        pay_client
            .try_release_escrow(&env.current_contract_address(), &wf.payment_id)
            .map_err(|_| CoordinatorError::PaymentUpdateFailed)?
            .map_err(|_| CoordinatorError::PaymentUpdateFailed)?;

        wf.status = WorkflowStatus::Settled;
        save_workflow(&env, &wf);

        env.events().publish(
            (
                symbol_short!("coord"),
                symbol_short!("settld"),
                symbol_short!("v1"),
            ),
            (request_id, wf.payment_id),
        );

        Ok(())
    }

    /// Expire an allocated workflow after its allocation deadline.
    ///
    /// Anyone can call this once the deadline has passed. The function releases
    /// reserved units, refunds the locked payment, then marks the workflow
    /// expired. Cross-contract errors are propagated so the host transaction
    /// reverts instead of committing partial state.
    pub fn expire_workflow(env: Env, request_id: u64) -> Result<(), CoordinatorError> {
        Self::require_initialized(&env)?;
        Self::require_not_paused(&env)?;

        let mut wf = load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;

        if wf.status == WorkflowStatus::Expired {
            return Err(CoordinatorError::AlreadyDone);
        }

        if wf.status != WorkflowStatus::Allocated {
            return Err(CoordinatorError::InvalidWorkflowState);
        }

        let inv_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::InventoryContract)
            .unwrap();
        let inv_client = InventoryContractClient::new(&env, &inv_addr);

        let reservation = inv_client
            .try_get_reservation(&wf.reservation_id)
            .map_err(|_| CoordinatorError::InventoryUpdateFailed)?
            .map_err(|_| CoordinatorError::InventoryUpdateFailed)?;

        if env.ledger().timestamp() < reservation.expiration_timestamp {
            return Err(CoordinatorError::WorkflowNotExpired);
        }

        inv_client
            .try_release_reservation(&wf.reservation_id)
            .map_err(|_| CoordinatorError::InventoryUpdateFailed)?
            .map_err(|_| CoordinatorError::InventoryUpdateFailed)?;

        let pay_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::PaymentContract)
            .unwrap();
        let pay_client = PaymentContractClient::new(&env, &pay_addr);
        pay_client
            .try_refund_escrow(&env.current_contract_address(), &wf.payment_id)
            .map_err(|_| CoordinatorError::PaymentUpdateFailed)?
            .map_err(|_| CoordinatorError::PaymentUpdateFailed)?;

        wf.status = WorkflowStatus::Expired;
        wf.delivery_confirmed = false;
        save_workflow(&env, &wf);

        env.events().publish(
            (
                symbol_short!("coord"),
                symbol_short!("expird"),
                symbol_short!("v1"),
            ),
            (request_id, wf.payment_id),
        );

        Ok(())
    }

    /// Rollback – admin only. Releases units and refunds payment.
    ///
    /// Deliberately not gated by any pause flag: rollback is the canonical
    /// exit path and must remain callable under every pause combination
    /// (see `pause_flag`'s doc and `test_exit_always_invariant_over_flag_lattice`).
    pub fn rollback(env: Env, request_id: u64) -> Result<(), CoordinatorError> {
        get_admin(&env).require_auth();
        Self::require_initialized(&env)?;
        Self::require_compatible_domain_contracts(&env)?;

        let mut wf = load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)?;

        if wf.status == WorkflowStatus::Settled {
            return Err(CoordinatorError::CannotRollbackSettled);
        }

        let inv_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::InventoryContract)
            .unwrap();
        let inv_client = InventoryContractClient::new(&env, &inv_addr);
        // Best-effort, same as before: a workflow may have progressed past
        // Allocated (units no longer Reserved), so release_reservation's
        // per-unit "only touch Reserved units" guard makes this a safe no-op
        // for units that already moved on.
        let _ = inv_client.try_release_reservation(&wf.reservation_id);

        let pay_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::PaymentContract)
            .unwrap();
        let pay_client = PaymentContractClient::new(&env, &pay_addr);
        if let Ok(Ok(payment)) = pay_client.try_get_payment(&wf.payment_id) {
            if payment.status == PaymentStatus::Locked {
                let _ = pay_client.try_refund_escrow(&env.current_contract_address(), &wf.payment_id);
            }
        }

        wf.status = WorkflowStatus::RolledBack;
        save_workflow(&env, &wf);

        env.events().publish(
            (
                symbol_short!("coord"),
                symbol_short!("rollbk"),
                symbol_short!("v1"),
            ),
            request_id,
        );

        Ok(())
    }

    pub fn get_workflow(env: Env, request_id: u64) -> Result<WorkflowRecord, CoordinatorError> {
        load_workflow(&env, request_id).ok_or(CoordinatorError::WorkflowNotFound)
    }

    /// Flag a temperature breach: transitions the linked payment from Locked → Disputed.
    ///
    /// Called by the temperature contract when a sustained excursion is detected.
    ///
    /// # Errors
    /// - `PaymentNotFound`     - No payment with this ID
    /// - `InvalidPaymentState` - Payment is not in Locked status
    /// - `PaymentFlagFailed`   - Cross-contract call to payments failed
    pub fn flag_temperature_breach(
        env: Env,
        caller: Address,
        payment_id: u64,
        excursion_summary: ExcursionSummary,
    ) -> Result<(), CoordinatorError> {
        caller.require_auth();
        Self::require_initialized(&env)?;
        Self::require_not_paused(&env)?;
        Self::require_compatible_domain_contracts(&env)?;

        let pay_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::PaymentContract)
            .unwrap();
        let pay_client = PaymentContractClient::new(&env, &pay_addr);

        let payment = pay_client
            .try_get_payment(&payment_id)
            .map_err(|_| CoordinatorError::PaymentNotFound)?
            .map_err(|_| CoordinatorError::PaymentNotFound)?;

        if payment.status != PaymentStatus::Locked {
            return Err(CoordinatorError::InvalidPaymentState);
        }

        let case_id = String::from_str(&env, "TEMP-EXCURSION");

        pay_client
            .try_record_dispute(
                &env.current_contract_address(),
                &payment_id,
                &DisputeReason::TemperatureExcursion,
                &case_id,
            )
            .map_err(|_| CoordinatorError::PaymentFlagFailed)?
            .map_err(|_| CoordinatorError::PaymentFlagFailed)?;

        let now = env.ledger().timestamp();
        env.events().publish(
            (symbol_short!("coord"), symbol_short!("tmp_brch")),
            (payment_id, excursion_summary.unit_id, now),
        );

        Ok(())
    }

    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Admin)
    }

    fn require_initialized(env: &Env) -> Result<(), CoordinatorError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(CoordinatorError::NotInitialized);
        }
        Ok(())
    }

    /// Fail closed when any domain contract runs an unsupported code
    /// version, so a partially-upgraded system never mis-executes (#31).
    fn require_compatible_domain_contracts(env: &Env) -> Result<(), CoordinatorError> {
        let requests: Address = env
            .storage()
            .instance()
            .get(&DataKey::RequestContract)
            .ok_or(CoordinatorError::NotInitialized)?;
        let inventory: Address = env
            .storage()
            .instance()
            .get(&DataKey::InventoryContract)
            .ok_or(CoordinatorError::NotInitialized)?;
        let payments: Address = env
            .storage()
            .instance()
            .get(&DataKey::PaymentContract)
            .ok_or(CoordinatorError::NotInitialized)?;
        require_supported_version(env, &requests)?;
        require_supported_version(env, &inventory)?;
        require_supported_version(env, &payments)?;
        Ok(())
    }

    // ── Timelocked upgradeability & versioned storage schema (#31) ───────────

    /// Code version of the currently deployed binary.
    pub fn version(_env: Env) -> u32 {
        CONTRACT_VERSION
    }

    /// Storage schema version currently recorded on-chain (1 when unset).
    pub fn schema_version(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&SCHEMA_VERSION_KEY)
            .unwrap_or(1)
    }

    /// Propose replacing the running WASM. Admin only. This contract holds
    /// (or orchestrates) escrowed funds, so the upgrade only becomes
    /// executable after `UPGRADE_TIMELOCK_SECS` — it can never be swapped
    /// instantly. Returns the ledger timestamp at which `execute_upgrade`
    /// becomes callable.
    pub fn propose_upgrade(
        env: Env,
        new_wasm_hash: soroban_sdk::BytesN<32>,
    ) -> Result<u64, CoordinatorError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(CoordinatorError::Unauthorized)?;
        admin.require_auth();
        if env.storage().instance().has(&PENDING_UPGRADE_KEY) {
            return Err(CoordinatorError::UpgradeAlreadyPending);
        }
        let now = env.ledger().timestamp();
        let pending = PendingUpgrade {
            new_wasm_hash,
            proposed_at: now,
            executable_at: now + UPGRADE_TIMELOCK_SECS,
        };
        env.storage().instance().set(&PENDING_UPGRADE_KEY, &pending);
        events::emit_upgrade_proposed(&env, pending.executable_at);
        Ok(pending.executable_at)
    }

    /// Cancel the pending upgrade proposal. Admin only.
    pub fn cancel_upgrade(env: Env) -> Result<(), CoordinatorError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(CoordinatorError::Unauthorized)?;
        admin.require_auth();
        if !env.storage().instance().has(&PENDING_UPGRADE_KEY) {
            return Err(CoordinatorError::NoPendingUpgrade);
        }
        env.storage().instance().remove(&PENDING_UPGRADE_KEY);
        events::emit_upgrade_canceled(&env);
        Ok(())
    }

    /// The currently pending upgrade proposal, if any.
    pub fn get_pending_upgrade(env: Env) -> Option<PendingUpgrade> {
        env.storage().instance().get(&PENDING_UPGRADE_KEY)
    }

    /// Execute the proposed upgrade once its timelock has elapsed. Admin
    /// only. The contract ID and all storage are preserved; call `migrate`
    /// afterwards when the new binary bumps `TARGET_SCHEMA_VERSION`.
    pub fn execute_upgrade(env: Env) -> Result<(), CoordinatorError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(CoordinatorError::Unauthorized)?;
        admin.require_auth();
        let pending: PendingUpgrade = env
            .storage()
            .instance()
            .get(&PENDING_UPGRADE_KEY)
            .ok_or(CoordinatorError::NoPendingUpgrade)?;
        if env.ledger().timestamp() < pending.executable_at {
            return Err(CoordinatorError::TimelockNotElapsed);
        }
        env.storage().instance().remove(&PENDING_UPGRADE_KEY);
        env.events()
            .publish((symbol_short!("upgrade"), symbol_short!("executed")), ());
        env.deployer()
            .update_current_contract_wasm(pending.new_wasm_hash);
        Ok(())
    }

    /// Apply version-gated storage migrations after an upgrade. Admin only.
    /// Refuses to run once storage already sits at `TARGET_SCHEMA_VERSION`,
    /// so a migration can never be applied twice.
    pub fn migrate(env: Env) -> Result<u32, CoordinatorError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(CoordinatorError::Unauthorized)?;
        admin.require_auth();
        let current = Self::schema_version(env.clone());
        if current >= TARGET_SCHEMA_VERSION {
            return Err(CoordinatorError::MigrationAlreadyApplied);
        }
        // Version-gated transformations run here as the schema evolves, e.g.
        // `if current < 2 { /* rewrite v1 entries into the v2 layout */ }`.
        env.storage()
            .instance()
            .set(&SCHEMA_VERSION_KEY, &TARGET_SCHEMA_VERSION);
        Ok(TARGET_SCHEMA_VERSION)
    }
}

// ── Upgradeability & versioned storage schema (#31) ───────────────────────────
//
// Invariant: after an upgrade, the new binary must be able to read every
// prior storage schema version until `migrate` has completed. Absence of the
// stored schema version means schema 1.

/// Code version compiled into this binary. Bump on every release.
pub const CONTRACT_VERSION: u32 = 1;

/// Storage schema version this binary writes. Bump only together with a
/// version-gated transformation in `migrate`.
pub const TARGET_SCHEMA_VERSION: u32 = 1;

const SCHEMA_VERSION_KEY: soroban_sdk::Symbol = soroban_sdk::symbol_short!("SCHEMA_V");

/// Delay between proposing and executing a WASM upgrade (48 hours). Applies
/// because this contract participates in custody of escrowed donor funds.
pub const UPGRADE_TIMELOCK_SECS: u64 = 172_800;

const PENDING_UPGRADE_KEY: soroban_sdk::Symbol = soroban_sdk::symbol_short!("PEND_UPG");

/// A queued WASM upgrade awaiting its timelock window.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingUpgrade {
    pub new_wasm_hash: soroban_sdk::BytesN<32>,
    pub proposed_at: u64,
    pub executable_at: u64,
}

// ── Cross-contract version compatibility (#31) ────────────────────────────────

/// Domain-contract code versions this coordinator can safely orchestrate.
pub const MIN_SUPPORTED_DOMAIN_VERSION: u32 = 1;
pub const MAX_SUPPORTED_DOMAIN_VERSION: u32 = 1;

mod version_client {
    use soroban_sdk::{contractclient, Env};

    #[contractclient(name = "VersionedContractClient")]
    pub trait VersionedContract {
        fn version(env: Env) -> u32;
    }
}

fn require_supported_version(env: &Env, contract: &Address) -> Result<(), CoordinatorError> {
    let version = version_client::VersionedContractClient::new(env, contract)
        .try_version()
        .map_err(|_| CoordinatorError::IncompatibleContractVersion)?
        .map_err(|_| CoordinatorError::IncompatibleContractVersion)?;
    if !(MIN_SUPPORTED_DOMAIN_VERSION..=MAX_SUPPORTED_DOMAIN_VERSION).contains(&version) {
        return Err(CoordinatorError::IncompatibleContractVersion);
    }
    Ok(())
}
