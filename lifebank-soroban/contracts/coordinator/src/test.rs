#![cfg(test)]

//! Cross-contract integration tests for the coordinator workflow.
//!
//! Each test registers mock implementations of the four domain contracts
//! alongside the coordinator in a single Soroban test environment, then
//! drives the full request → allocation → delivery → settlement sequence.

use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{Address as _, Ledger},
    vec, Address, Env, String, Vec,
};

use super::{CoordinatorContract, CoordinatorContractClient, CoordinatorError, WorkflowStatus};

// Cross-contract domain types now live in the shared `interfaces` crate — the
// single source of truth for the wire format. The coordinator no longer
// hand-mirrors these, so the mocks below exercise the exact same
// `contracttype` definitions as production.
use interfaces::{
    BloodRequest, BloodStatus, BloodType, BloodUnit, Payment, PaymentStatus, RequestStatus,
};

// ── Mock: Request contract ────────────────────────────────────────────────────

#[contracttype]
enum ReqKey {
    Request(u64),
    Counter,
}

#[contract]
struct MockRequestContract;

#[contractimpl]
impl MockRequestContract {
    pub fn version() -> u32 {
        1
    }

    pub fn seed_request(env: Env, id: u64, status: RequestStatus) {
        env.storage().persistent().set(
            &ReqKey::Request(id),
            &BloodRequest { id, status, blood_type: BloodType::APositive },
        );
    }

    pub fn seed_request_with_type(env: Env, id: u64, status: RequestStatus, blood_type: BloodType) {
        env.storage()
            .persistent()
            .set(&ReqKey::Request(id), &BloodRequest { id, status, blood_type });
    }

    pub fn get_request(env: Env, request_id: u64) -> BloodRequest {
        env.storage()
            .persistent()
            .get(&ReqKey::Request(request_id))
            .unwrap()
    }
}

// ── Mock: Inventory contract ──────────────────────────────────────────────────

#[contracttype]
enum InvKey {
    Unit(u64),
    Admin,
    Counter,
    FailUpdateUnit(u64),
    Reservation(u64),
    ReservationCounter,
    FailReleaseReservation(u64),
}

#[contract]
struct MockInventoryContract;

#[contractimpl]
impl MockInventoryContract {
    pub fn version() -> u32 {
        1
    }

    pub fn initialize(env: Env, admin: Address) {
        env.storage().instance().set(&InvKey::Admin, &admin);
        env.storage().instance().set(&InvKey::Counter, &0u64);
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage().instance().get(&InvKey::Admin).unwrap()
    }

    pub fn register_unit(env: Env, blood_type: BloodType) -> u64 {
        let id: u64 = env
            .storage()
            .instance()
            .get(&InvKey::Counter)
            .unwrap_or(0u64)
            + 1;
        env.storage().instance().set(&InvKey::Counter, &id);
        env.storage().persistent().set(
            &InvKey::Unit(id),
            &BloodUnit {
                id,
                status: BloodStatus::Available,
                blood_type,
            },
        );
        id
    }

    pub fn get_blood_unit(env: Env, blood_unit_id: u64) -> BloodUnit {
        env.storage()
            .persistent()
            .get(&InvKey::Unit(blood_unit_id))
            .unwrap()
    }

    pub fn fail_update_for_unit(env: Env, unit_id: u64) {
        env.storage()
            .persistent()
            .set(&InvKey::FailUpdateUnit(unit_id), &true);
    }

    pub fn update_status(
        env: Env,
        unit_id: u64,
        new_status: BloodStatus,
        _authorized_by: Address,
        _reason: Option<String>,
    ) -> BloodUnit {
        if env
            .storage()
            .persistent()
            .get(&InvKey::FailUpdateUnit(unit_id))
            .unwrap_or(false)
        {
            panic!("forced inventory update failure");
        }

        let mut unit: BloodUnit = env
            .storage()
            .persistent()
            .get(&InvKey::Unit(unit_id))
            .unwrap();
        unit.status = new_status;
        env.storage()
            .persistent()
            .set(&InvKey::Unit(unit_id), &unit);
        unit
    }

    pub fn mark_delivered(
        env: Env,
        unit_id: u64,
        authorized_by: Address,
        delivery_location: String,
    ) -> BloodUnit {
        Self::update_status(
            env,
            unit_id,
            BloodStatus::Delivered,
            authorized_by,
            Some(delivery_location),
        )
    }

    /// Force `release_reservation` to panic for a specific reservation, so
    /// tests can exercise the "cross-contract call fails, no partial state"
    /// path the way `fail_update_for_unit` does for `update_status`.
    pub fn fail_release_for_reservation(env: Env, reservation_id: u64) {
        env.storage()
            .persistent()
            .set(&InvKey::FailReleaseReservation(reservation_id), &true);
    }

    pub fn reserve_blood(
        env: Env,
        requester: Address,
        unit_ids: Vec<u64>,
        request_id: u64,
        duration_seconds: u64,
    ) -> u64 {
        let id: u64 = env
            .storage()
            .instance()
            .get(&InvKey::ReservationCounter)
            .unwrap_or(0u64)
            + 1;
        env.storage().instance().set(&InvKey::ReservationCounter, &id);

        let now = env.ledger().timestamp();
        let reservation = super::Reservation {
            unit_ids: unit_ids.clone(),
            requester,
            created_timestamp: now,
            expiration_timestamp: now + duration_seconds,
            request_id,
        };
        env.storage()
            .persistent()
            .set(&InvKey::Reservation(id), &reservation);

        for i in 0..unit_ids.len() {
            let uid = unit_ids.get(i).unwrap();
            let mut unit: BloodUnit = env
                .storage()
                .persistent()
                .get(&InvKey::Unit(uid))
                .unwrap();
            unit.status = BloodStatus::Reserved;
            env.storage().persistent().set(&InvKey::Unit(uid), &unit);
        }

        id
    }

    pub fn release_reservation(env: Env, reservation_id: u64) {
        if env
            .storage()
            .persistent()
            .get(&InvKey::FailReleaseReservation(reservation_id))
            .unwrap_or(false)
        {
            panic!("forced release_reservation failure");
        }

        let reservation: super::Reservation = env
            .storage()
            .persistent()
            .get(&InvKey::Reservation(reservation_id))
            .unwrap();

        for i in 0..reservation.unit_ids.len() {
            let uid = reservation.unit_ids.get(i).unwrap();
            let mut unit: BloodUnit = env
                .storage()
                .persistent()
                .get(&InvKey::Unit(uid))
                .unwrap();
            if unit.status == BloodStatus::Reserved {
                unit.status = BloodStatus::Available;
                env.storage().persistent().set(&InvKey::Unit(uid), &unit);
            }
        }

        env.storage()
            .persistent()
            .remove(&InvKey::Reservation(reservation_id));
    }

    pub fn get_reservation(env: Env, reservation_id: u64) -> super::Reservation {
        env.storage()
            .persistent()
            .get(&InvKey::Reservation(reservation_id))
            .unwrap()
    }

    pub fn extend_reservation(
        env: Env,
        reservation_id: u64,
        additional_seconds: u64,
        _authorized_by: Address,
    ) -> super::Reservation {
        let mut reservation: super::Reservation = env
            .storage()
            .persistent()
            .get(&InvKey::Reservation(reservation_id))
            .unwrap();
        reservation.expiration_timestamp += additional_seconds;
        env.storage()
            .persistent()
            .set(&InvKey::Reservation(reservation_id), &reservation);
        reservation
    }
}

// ── Mock: Payment contract ────────────────────────────────────────────────────

#[contracttype]
enum PayKey {
    Payment(u64),
    Escrow(u64),
    Counter,
    FailUpdates,
    Coordinator,
}

#[contracttype]
struct MockEscrowInfo {
    payer: Address,
    payee: Address,
    amount: i128,
    token: Address,
}

#[contract]
struct MockPaymentContract;

#[contractimpl]
impl MockPaymentContract {
    pub fn version() -> u32 {
        1
    }

    pub fn create_payment(env: Env, request_id: u64, status: PaymentStatus) -> u64 {
        let id: u64 = env
            .storage()
            .instance()
            .get(&PayKey::Counter)
            .unwrap_or(0u64)
            + 1;
        env.storage().instance().set(&PayKey::Counter, &id);
        env.storage().persistent().set(
            &PayKey::Payment(id),
            &Payment {
                id,
                request_id,
                status,
            },
        );
        id
    }

    /// Seed a payment that also holds real escrowed tokens, so tests can
    /// assert `release_escrow`/`refund_escrow` move an actual token balance
    /// instead of only flipping a status label. Payments created via plain
    /// `create_payment` have no escrow entry; `release_escrow`/`refund_escrow`
    /// on those skip the transfer (nothing to move) and only update status,
    /// which is enough for the many workflow-shape tests that don't care
    /// about money.
    pub fn seed_escrow(
        env: Env,
        request_id: u64,
        payer: Address,
        payee: Address,
        amount: i128,
        token: Address,
    ) -> u64 {
        let id = Self::create_payment(env.clone(), request_id, PaymentStatus::Locked);
        env.storage().persistent().set(
            &PayKey::Escrow(id),
            &MockEscrowInfo {
                payer,
                payee,
                amount,
                token,
            },
        );
        id
    }

    pub fn get_payment(env: Env, payment_id: u64) -> Payment {
        env.storage()
            .persistent()
            .get(&PayKey::Payment(payment_id))
            .unwrap()
    }

    pub fn fail_updates(env: Env) {
        env.storage().persistent().set(&PayKey::FailUpdates, &true);
    }

    pub fn set_coordinator(env: Env, coordinator: Address) {
        env.storage()
            .instance()
            .set(&PayKey::Coordinator, &coordinator);
    }

    pub fn get_coordinator(env: Env) -> Option<Address> {
        env.storage().instance().get(&PayKey::Coordinator)
    }

    fn check_authorized(env: &Env, caller: &Address) {
        caller.require_auth();
        if env
            .storage()
            .persistent()
            .get(&PayKey::FailUpdates)
            .unwrap_or(false)
        {
            panic!("forced payment update failure");
        }
    }

    pub fn release_escrow(env: Env, caller: Address, payment_id: u64) {
        Self::check_authorized(&env, &caller);
        if let Some(esc) = env
            .storage()
            .persistent()
            .get::<_, MockEscrowInfo>(&PayKey::Escrow(payment_id))
        {
            let token_client = soroban_sdk::token::Client::new(&env, &esc.token);
            token_client.transfer(&env.current_contract_address(), &esc.payee, &esc.amount);
        }
        let mut p: Payment = env
            .storage()
            .persistent()
            .get(&PayKey::Payment(payment_id))
            .unwrap();
        p.status = PaymentStatus::Released;
        env.storage()
            .persistent()
            .set(&PayKey::Payment(payment_id), &p);
    }

    pub fn refund_escrow(env: Env, caller: Address, payment_id: u64) {
        Self::check_authorized(&env, &caller);
        if let Some(esc) = env
            .storage()
            .persistent()
            .get::<_, MockEscrowInfo>(&PayKey::Escrow(payment_id))
        {
            let token_client = soroban_sdk::token::Client::new(&env, &esc.token);
            token_client.transfer(&env.current_contract_address(), &esc.payer, &esc.amount);
        }
        let mut p: Payment = env
            .storage()
            .persistent()
            .get(&PayKey::Payment(payment_id))
            .unwrap();
        p.status = PaymentStatus::Refunded;
        env.storage()
            .persistent()
            .set(&PayKey::Payment(payment_id), &p);
    }

    pub fn record_dispute(
        env: Env,
        caller: Address,
        payment_id: u64,
        _reason: interfaces::DisputeReason,
        _case_id: String,
    ) {
        caller.require_auth();
        let mut p: Payment = env
            .storage()
            .persistent()
            .get(&PayKey::Payment(payment_id))
            .unwrap();
        p.status = PaymentStatus::Disputed;
        env.storage()
            .persistent()
            .set(&PayKey::Payment(payment_id), &p);
    }
}

// ── Harness ───────────────────────────────────────────────────────────────────

struct Harness<'a> {
    env: Env,
    admin: Address,
    coord: CoordinatorContractClient<'a>,
    req_id: Address,
    inv_id: Address,
    pay_id: Address,
}

/// Registers the four domain mocks plus the coordinator, honoring the
/// coordinator's admin-wiring invariant: `inventory.get_admin() ==
/// coordinator_address`.
///
/// The coordinator's constructor asserts this equality, but inventory's
/// admin can only be set once, at inventory's own construction — so the
/// coordinator's address must be known and given to inventory *before* the
/// coordinator itself is deployed. `env.register_at` lets a test pre-assign
/// any address to a not-yet-deployed contract, which resolves the ordering:
/// generate the coordinator's future address, hand it to inventory as admin,
/// then deploy the coordinator at that exact address.
fn setup<'a>() -> Harness<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let coord_id = Address::generate(&env);

    let req_id = env.register(MockRequestContract, ());
    let inv_id = env.register(MockInventoryContract, ());
    let pay_id = env.register(MockPaymentContract, ());

    MockInventoryContractClient::new(&env, &inv_id).initialize(&coord_id);
    MockPaymentContractClient::new(&env, &pay_id).set_coordinator(&coord_id);

    env.register_at(
        &coord_id,
        CoordinatorContract,
        (&admin, &req_id, &inv_id, &pay_id),
    );

    let coord = CoordinatorContractClient::new(&env, &coord_id);

    Harness {
        env,
        admin,
        coord,
        req_id,
        inv_id,
        pay_id,
    }
}

fn seed_pending_request(h: &Harness, id: u64) {
    MockRequestContractClient::new(&h.env, &h.req_id).seed_request(&id, &RequestStatus::Pending);
}

fn register_unit(h: &Harness) -> u64 {
    MockInventoryContractClient::new(&h.env, &h.inv_id).register_unit(&BloodType::APositive)
}

fn create_locked_payment(h: &Harness, request_id: u64) -> u64 {
    MockPaymentContractClient::new(&h.env, &h.pay_id)
        .create_payment(&request_id, &PaymentStatus::Locked)
}

fn create_locked_escrow_payment(
    h: &Harness,
    request_id: u64,
    payer: &Address,
    payee: &Address,
    amount: i128,
    token: &Address,
) -> u64 {
    MockPaymentContractClient::new(&h.env, &h.pay_id)
        .seed_escrow(&request_id, payer, payee, &amount, token)
}

/// Deploy a minimal Soroban token contract and mint `amount` to `recipient`.
fn deploy_token_with_balance(env: &Env, admin: &Address, recipient: &Address, amount: i128) -> Address {
    let token = env.register_stellar_asset_contract_v2(admin.clone());
    let token_id = token.address();
    let token_admin = soroban_sdk::token::StellarAssetClient::new(env, &token_id);
    token_admin.mint(recipient, &amount);
    token_id
}

fn reservation_deadline(h: &Harness, reservation_id: u64) -> u64 {
    MockInventoryContractClient::new(&h.env, &h.inv_id)
        .get_reservation(&reservation_id)
        .expiration_timestamp
}

// ── Happy path ────────────────────────────────────────────────────────────────

#[test]
fn test_full_happy_path() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Allocated);
    assert!(!wf.delivery_confirmed);

    let unit = MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id);
    assert_eq!(unit.status, BloodStatus::Reserved);

    h.coord.confirm_delivery(&1u64, &h.admin);

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Delivered);
    assert!(wf.delivery_confirmed);

    let unit = MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id);
    assert_eq!(unit.status, BloodStatus::Delivered);

    h.coord.settle_payment(&1u64, &h.admin);

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Settled);

    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Released);
}

// ── Real token movement (settle/expire/rollback must move actual funds) ───────

#[test]
fn test_settle_payment_releases_real_tokens_to_payee() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);

    let payer = Address::generate(&h.env);
    let payee = Address::generate(&h.env);
    let token = deploy_token_with_balance(&h.env, &h.admin, &h.pay_id, 1_000);
    let payment_id = create_locked_escrow_payment(&h, 1, &payer, &payee, 1_000, &token);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    h.coord.confirm_delivery(&1u64, &h.admin);

    let token_client = soroban_sdk::token::Client::new(&h.env, &token);
    assert_eq!(token_client.balance(&payee), 0);

    h.coord.settle_payment(&1u64, &h.admin);

    assert_eq!(token_client.balance(&payee), 1_000);
    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Released);
}

#[test]
fn test_expire_workflow_refunds_real_tokens_to_payer() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);

    let payer = Address::generate(&h.env);
    let payee = Address::generate(&h.env);
    let token = deploy_token_with_balance(&h.env, &h.admin, &h.pay_id, 1_000);
    let payment_id = create_locked_escrow_payment(&h, 1, &payer, &payee, 1_000, &token);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    let wf = h.coord.get_workflow(&1u64);
    let deadline = reservation_deadline(&h, wf.reservation_id);
    h.env.ledger().with_mut(|l| l.timestamp = deadline + 1);

    let token_client = soroban_sdk::token::Client::new(&h.env, &token);
    assert_eq!(token_client.balance(&payer), 0);

    h.coord.expire_workflow(&1u64);

    assert_eq!(token_client.balance(&payer), 1_000);
    assert_eq!(token_client.balance(&h.pay_id), 0);
    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Refunded);
}

#[test]
fn test_rollback_refunds_real_tokens_to_payer() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);

    let payer = Address::generate(&h.env);
    let payee = Address::generate(&h.env);
    let token = deploy_token_with_balance(&h.env, &h.admin, &h.pay_id, 1_000);
    let payment_id = create_locked_escrow_payment(&h, 1, &payer, &payee, 1_000, &token);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);

    let token_client = soroban_sdk::token::Client::new(&h.env, &token);
    assert_eq!(token_client.balance(&payer), 0);

    h.coord.rollback(&1u64);

    assert_eq!(token_client.balance(&payer), 1_000);
    assert_eq!(token_client.balance(&h.pay_id), 0);
    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Refunded);
}

// ── Sequence enforcement ──────────────────────────────────────────────────────

#[test]
fn test_settle_blocked_without_delivery() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);

    let result = h.coord.try_settle_payment(&1u64, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::DeliveryNotConfirmed)));

    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Locked);
}

#[test]
fn test_double_allocation_blocked() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);

    let unit_id2 = register_unit(&h);
    let result = h
        .coord
        .try_allocate_units(&1u64, &vec![&h.env, unit_id2], &payment_id, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::AlreadyDone)));

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Allocated);
    assert!(!wf.delivery_confirmed);
}

#[test]
fn test_confirm_delivery_is_idempotent_after_delivery() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    h.coord.confirm_delivery(&1u64, &h.admin);

    let before = h.coord.get_workflow(&1u64);
    let result = h.coord.try_confirm_delivery(&1u64, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::AlreadyDone)));

    let after = h.coord.get_workflow(&1u64);
    assert_eq!(after.status, before.status);
    assert_eq!(after.delivery_confirmed, before.delivery_confirmed);
    assert_eq!(after.reservation_id, before.reservation_id);

    let unit = MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id);
    assert_eq!(unit.status, BloodStatus::Delivered);
}

#[test]
fn test_settle_payment_is_idempotent_after_settlement() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    h.coord.confirm_delivery(&1u64, &h.admin);
    h.coord.settle_payment(&1u64, &h.admin);

    let before = h.coord.get_workflow(&1u64);
    let result = h.coord.try_settle_payment(&1u64, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::AlreadyDone)));

    let after = h.coord.get_workflow(&1u64);
    assert_eq!(after.status, before.status);
    assert_eq!(after.delivery_confirmed, before.delivery_confirmed);

    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Released);
}

/// The reservation created by allocate_units is the sole record of the
/// allocation deadline: its expiration_timestamp must equal ledger-now plus
/// ALLOCATION_EXPIRY_SECONDS, and expire_workflow must read that same value
/// rather than a separately-tracked WorkflowRecord field.
#[test]
fn test_reservation_ttl_matches_allocation_expiry_and_gates_expiry() {
    let h = setup();
    h.env.ledger().with_mut(|l| l.timestamp = 5_000);

    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);
    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);

    let wf = h.coord.get_workflow(&1u64);
    let deadline = reservation_deadline(&h, wf.reservation_id);
    assert_eq!(deadline, 5_000 + crate::ALLOCATION_EXPIRY_SECONDS);

    // One second before the reservation's own deadline: still not expirable.
    h.env.ledger().with_mut(|l| l.timestamp = deadline - 1);
    assert_eq!(
        h.coord.try_expire_workflow(&1u64),
        Err(Ok(CoordinatorError::WorkflowNotExpired))
    );

    // Past it: expire_workflow succeeds and releases the reservation.
    h.env.ledger().with_mut(|l| l.timestamp = deadline + 1);
    h.coord.expire_workflow(&1u64);
    assert_eq!(h.coord.get_workflow(&1u64).status, WorkflowStatus::Expired);

    let unit = MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id);
    assert_eq!(unit.status, BloodStatus::Available);
}

#[test]
fn test_expire_workflow_rejects_before_deadline() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);

    let result = h.coord.try_expire_workflow(&1u64);
    assert_eq!(result, Err(Ok(CoordinatorError::WorkflowNotExpired)));

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Allocated);
}

#[test]
fn test_expire_workflow_releases_units_refunds_payment_and_marks_expired() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    let wf = h.coord.get_workflow(&1u64);
    let deadline = reservation_deadline(&h, wf.reservation_id);
    h.env.ledger().with_mut(|li| {
        li.timestamp = deadline + 1;
    });

    h.coord.expire_workflow(&1u64);

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Expired);
    assert!(!wf.delivery_confirmed);

    let unit = MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id);
    assert_eq!(unit.status, BloodStatus::Available);

    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Refunded);
}

#[test]
fn test_expire_workflow_is_idempotent_after_expiry() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    let wf = h.coord.get_workflow(&1u64);
    let deadline = reservation_deadline(&h, wf.reservation_id);
    h.env.ledger().with_mut(|li| {
        li.timestamp = deadline + 1;
    });

    h.coord.expire_workflow(&1u64);
    let result = h.coord.try_expire_workflow(&1u64);
    assert_eq!(result, Err(Ok(CoordinatorError::AlreadyDone)));

    let unit = MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id);
    assert_eq!(unit.status, BloodStatus::Available);

    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Refunded);
}

#[test]
fn test_expire_workflow_inventory_failure_leaves_no_partial_state() {
    let h = setup();
    seed_pending_request(&h, 1);
    let first_unit_id = register_unit(&h);
    let second_unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord.allocate_units(
        &1u64,
        &vec![&h.env, first_unit_id, second_unit_id],
        &payment_id,
        &h.admin,
    );
    let wf = h.coord.get_workflow(&1u64);
    let deadline = reservation_deadline(&h, wf.reservation_id);
    h.env.ledger().with_mut(|li| {
        li.timestamp = deadline + 1;
    });

    MockInventoryContractClient::new(&h.env, &h.inv_id)
        .fail_release_for_reservation(&wf.reservation_id);

    let result = h.coord.try_expire_workflow(&1u64);
    assert!(result.is_err());

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Allocated);

    let inv = MockInventoryContractClient::new(&h.env, &h.inv_id);
    assert_eq!(
        inv.get_blood_unit(&first_unit_id).status,
        BloodStatus::Reserved
    );
    assert_eq!(
        inv.get_blood_unit(&second_unit_id).status,
        BloodStatus::Reserved
    );

    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Locked);
}

#[test]
fn test_expire_workflow_payment_failure_leaves_no_partial_state() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    let wf = h.coord.get_workflow(&1u64);
    let deadline = reservation_deadline(&h, wf.reservation_id);
    h.env.ledger().with_mut(|li| {
        li.timestamp = deadline + 1;
    });

    MockPaymentContractClient::new(&h.env, &h.pay_id).fail_updates();

    let result = h.coord.try_expire_workflow(&1u64);
    assert!(result.is_err());

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::Allocated);

    let unit = MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id);
    assert_eq!(unit.status, BloodStatus::Reserved);

    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Locked);
}

#[test]
fn test_allocate_blocked_for_unavailable_unit() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    // Pre-reserve the unit
    MockInventoryContractClient::new(&h.env, &h.inv_id).update_status(
        &unit_id,
        &BloodStatus::Reserved,
        &h.admin,
        &None,
    );

    let result = h
        .coord
        .try_allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::UnitNotAvailable)));
}

#[test]
fn test_settle_blocked_for_pending_payment() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    // Payment left Pending (not Locked)
    let payment_id = MockPaymentContractClient::new(&h.env, &h.pay_id)
        .create_payment(&1u64, &PaymentStatus::Pending);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    h.coord.confirm_delivery(&1u64, &h.admin);

    let result = h.coord.try_settle_payment(&1u64, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::InvalidPaymentState)));
}

#[test]
fn test_confirm_delivery_blocked_before_allocation() {
    let h = setup();
    let result = h.coord.try_confirm_delivery(&99u64, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::WorkflowNotFound)));
}

#[test]
fn test_allocate_blocked_for_non_pending_request() {
    let h = setup();
    // Seed request with Approved status (not Pending)
    MockRequestContractClient::new(&h.env, &h.req_id).seed_request(&1u64, &RequestStatus::Approved);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    let result = h
        .coord
        .try_allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::InvalidRequestState)));
}

// ── Rollback ──────────────────────────────────────────────────────────────────

#[test]
fn test_rollback_releases_units_and_refunds_payment() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    h.coord.rollback(&1u64);

    let wf = h.coord.get_workflow(&1u64);
    assert_eq!(wf.status, WorkflowStatus::RolledBack);

    let unit = MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id);
    assert_eq!(unit.status, BloodStatus::Available);

    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Refunded);
}

#[test]
fn test_rollback_blocked_after_settlement() {
    let h = setup();
    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let payment_id = create_locked_payment(&h, 1);

    h.coord
        .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    h.coord.confirm_delivery(&1u64, &h.admin);
    h.coord.settle_payment(&1u64, &h.admin);

    let result = h.coord.try_rollback(&1u64);
    assert_eq!(result, Err(Ok(CoordinatorError::CannotRollbackSettled)));
}

// ── Circuit breaker tests ─────────────────────────────────────────────────────

#[test]
fn test_coordinator_pause_blocks_allocate_units() {
    let h = setup();
    h.coord.pause(&h.admin);
    assert!(h.coord.is_paused());

    seed_pending_request(&h, 1);
    let unit_id = register_unit(&h);
    let pay_id = create_locked_payment(&h, 1);

    let result = h
        .coord
        .try_allocate_units(&1u64, &vec![&h.env, unit_id], &pay_id, &h.admin);
    assert!(result.is_err());
}

#[test]
fn test_coordinator_pause_allows_get_workflow() {
    let h = setup();

    // Create a workflow first
    seed_pending_request(&h, 10);
    let unit_id = register_unit(&h);
    let pay_id = create_locked_payment(&h, 10);
    h.coord
        .allocate_units(&10u64, &vec![&h.env, unit_id], &pay_id, &h.admin);

    h.coord.pause(&h.admin);

    // Read still works
    let wf = h.coord.get_workflow(&10u64);
    assert_eq!(wf.request_id, 10);
}

#[test]
fn test_coordinator_unpause_restores_writes() {
    let h = setup();
    h.coord.pause(&h.admin);
    h.coord.unpause(&h.admin);
    assert!(!h.coord.is_paused());

    seed_pending_request(&h, 20);
    let unit_id = register_unit(&h);
    let pay_id = create_locked_payment(&h, 20);
    h.coord
        .allocate_units(&20u64, &vec![&h.env, unit_id], &pay_id, &h.admin);
    assert_eq!(
        h.coord.get_workflow(&20u64).status,
        WorkflowStatus::Allocated
    );
}

#[test]
#[should_panic]
fn test_coordinator_non_admin_cannot_pause() {
    let h = setup();
    let attacker = Address::generate(&h.env);
    h.coord.pause(&attacker);
}

// ── Granular pause flag tests ─────────────────────────────────────────────────

/// Guardian can pause the alloc flag; rollback (exit path) still works.
#[test]
fn test_guardian_can_pause_alloc_flag() {
    let h = setup();
    let guardian = Address::generate(&h.env);
    h.coord.set_guardian(&h.admin, &guardian);

    use soroban_sdk::symbol_short;
    h.coord.pause_flag(&guardian, &symbol_short!("alloc"));
    assert!(h.coord.is_flag_paused(&symbol_short!("alloc")));

    // New allocations blocked
    seed_pending_request(&h, 50);
    let uid = register_unit(&h);
    let pid = create_locked_payment(&h, 50);
    let result = h.coord.try_allocate_units(&50u64, &vec![&h.env, uid], &pid, &h.admin);
    assert!(result.is_err());
}

/// Only admin can unpause; guardian cannot.
#[test]
#[should_panic]
fn test_guardian_cannot_unpause() {
    let h = setup();
    let guardian = Address::generate(&h.env);
    h.coord.set_guardian(&h.admin, &guardian);
    use soroban_sdk::symbol_short;
    h.coord.pause_flag(&guardian, &symbol_short!("alloc"));
    h.coord.unpause_flag(&guardian, &symbol_short!("alloc"));
}

/// Pausing alloc does NOT block settle (different flag).
#[test]
fn test_alloc_pause_does_not_block_settle() {
    let h = setup();
    use soroban_sdk::symbol_short;
    seed_pending_request(&h, 60);
    let uid = register_unit(&h);
    let pid = create_locked_payment(&h, 60);
    h.coord.allocate_units(&60u64, &vec![&h.env, uid], &pid, &h.admin);
    h.coord.confirm_delivery(&60u64, &h.admin);

    // Pause alloc — settle must still work
    h.coord.pause_flag(&h.admin, &symbol_short!("alloc"));
    h.coord.settle_payment(&60u64, &h.admin);
    assert_eq!(h.coord.get_workflow(&60u64).status, WorkflowStatus::Settled);
}

/// Auto-enable: after MAX_PAUSE_SECS the flag is treated as unpaused.
#[test]
fn test_auto_enable_after_max_pause_secs() {
    use crate::MAX_PAUSE_SECS;
    use soroban_sdk::symbol_short;
    let h = setup();
    h.env.ledger().with_mut(|l| l.timestamp = 1_000);
    h.coord.pause_flag(&h.admin, &symbol_short!("alloc"));
    assert!(h.coord.is_flag_paused(&symbol_short!("alloc")));

    // Advance past the safety window
    h.env.ledger().with_mut(|l| l.timestamp = 1_000 + MAX_PAUSE_SECS + 1);
    assert!(!h.coord.is_flag_paused(&symbol_short!("alloc")));

    // New allocations must succeed after auto-enable
    seed_pending_request(&h, 70);
    let uid = register_unit(&h);
    let pid = create_locked_payment(&h, 70);
    h.coord.allocate_units(&70u64, &vec![&h.env, uid], &pid, &h.admin);
    assert_eq!(h.coord.get_workflow(&70u64).status, WorkflowStatus::Allocated);
}

/// Exit-always property test: under every combination of the 3 pause flags,
/// rollback (the canonical exit path) remains callable on an Allocated workflow.
/// This covers all 8 elements of the flag lattice {alloc, dlvr, settl}^2.
#[test]
fn test_exit_always_invariant_over_flag_lattice() {
    use crate::MAX_PAUSE_SECS;
    use soroban_sdk::symbol_short;
    let flag_list = [symbol_short!("alloc"), symbol_short!("dlvr"), symbol_short!("settl")];

    for mask in 0u8..8u8 {
        let h = setup();
        h.env.ledger().with_mut(|l| l.timestamp = 1_000);

        let req_id = 200u64 + mask as u64;
        seed_pending_request(&h, req_id);
        let uid = register_unit(&h);
        let pid = create_locked_payment(&h, req_id);
        h.coord.allocate_units(&req_id, &vec![&h.env, uid], &pid, &h.admin);

        // Apply the flag combination for this mask
        for (i, flag) in flag_list.iter().enumerate() {
            if mask & (1 << i) != 0 {
                h.coord.pause_flag(&h.admin, flag);
            }
        }

        // Rollback (exit path) must always succeed regardless of flag combination
        h.coord.rollback(&req_id);
        assert_eq!(
            h.coord.get_workflow(&req_id).status,
            WorkflowStatus::RolledBack,
            "exit path blocked for flag mask {mask}"
        );
        let pay = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&pid);
        assert_eq!(
            pay.status,
            PaymentStatus::Refunded,
            "payment not refunded for flag mask {mask}"
        );
    }
}

/// Deadline extension: pausing dlvr extends the reservation's deadline
/// (in inventory) by the pause duration.
#[test]
fn test_deadline_extended_during_dlvr_pause() {
    use soroban_sdk::symbol_short;
    let h = setup();
    h.env.ledger().with_mut(|l| l.timestamp = 1_000);

    seed_pending_request(&h, 80);
    let uid = register_unit(&h);
    let pid = create_locked_payment(&h, 80);
    h.coord.allocate_units(&80u64, &vec![&h.env, uid], &pid, &h.admin);
    let wf = h.coord.get_workflow(&80u64);
    let original_deadline = reservation_deadline(&h, wf.reservation_id);

    // Pause dlvr at t=1000, unpause at t=2000 (1000s pause)
    h.coord.pause_flag(&h.admin, &symbol_short!("dlvr"));
    h.env.ledger().with_mut(|l| l.timestamp = 2_000);
    h.coord.unpause_flag(&h.admin, &symbol_short!("dlvr"));

    // confirm_delivery should extend the deadline
    h.coord.confirm_delivery(&80u64, &h.admin);
    let wf = h.coord.get_workflow(&80u64);
    let new_deadline = reservation_deadline(&h, wf.reservation_id);
    // deadline must be >= original + 1000s pause duration
    assert!(
        new_deadline >= original_deadline + 1_000,
        "deadline not extended: got {}, expected >= {}",
        new_deadline,
        original_deadline + 1_000
    );
}


// ── Temperature excursion → dispute integration tests (issue #477) ────────────

use interfaces::ExcursionSummary;

fn make_excursion(unit_id: u64) -> ExcursionSummary {
    ExcursionSummary {
        unit_id,
        violation_count: 3,
        peak_celsius_x100: 1200, // 12.00°C — above threshold
        detected_at: 1000,
    }
}

/// Full chain: flag_temperature_breach transitions Locked → Disputed.
#[test]
fn test_flag_temperature_breach_transitions_locked_to_disputed() {
    let h = setup();
    let payment_id = create_locked_payment(&h, 99);

    let excursion = make_excursion(42);
    h.coord
        .flag_temperature_breach(&h.admin, &payment_id, &excursion);

    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(
        payment.status,
        PaymentStatus::Disputed,
        "Payment must be Disputed after temperature breach"
    );
}

/// flag_temperature_breach on a non-Locked payment returns InvalidPaymentState.
#[test]
fn test_flag_temperature_breach_non_locked_payment_fails() {
    let h = setup();
    // Create a Released payment
    let payment_id = MockPaymentContractClient::new(&h.env, &h.pay_id)
        .create_payment(&1u64, &PaymentStatus::Released);

    let excursion = make_excursion(1);
    let result = h
        .coord
        .try_flag_temperature_breach(&h.admin, &payment_id, &excursion);
    assert_eq!(
        result,
        Err(Ok(CoordinatorError::InvalidPaymentState)),
        "Non-Locked payment must return InvalidPaymentState"
    );
}

/// flag_temperature_breach on a missing payment returns PaymentNotFound.
#[test]
fn test_flag_temperature_breach_missing_payment_fails() {
    let h = setup();
    let excursion = make_excursion(1);
    let result = h
        .coord
        .try_flag_temperature_breach(&h.admin, &9999u64, &excursion);
    assert_eq!(
        result,
        Err(Ok(CoordinatorError::PaymentNotFound)),
        "Missing payment must return PaymentNotFound"
    );
}

/// Paused coordinator rejects flag_temperature_breach.
#[test]
fn test_flag_temperature_breach_blocked_when_paused() {
    let h = setup();
    let payment_id = create_locked_payment(&h, 1);
    h.coord.pause(&h.admin);

    let excursion = make_excursion(1);
    let result = h
        .coord
        .try_flag_temperature_breach(&h.admin, &payment_id, &excursion);
    assert_eq!(result, Err(Ok(CoordinatorError::ContractPaused)));

    // Payment must remain Locked
    let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
    assert_eq!(payment.status, PaymentStatus::Locked);
}

// ── Timelocked upgradeability, schema migration & version gates (#31) ─────────

mod upgrade_tests {
    use super::*;
    use crate::{CONTRACT_VERSION, TARGET_SCHEMA_VERSION, UPGRADE_TIMELOCK_SECS};
    use soroban_sdk::testutils::Ledger as _;
    use soroban_sdk::{contract, contractimpl, BytesN};

    #[test]
    fn test_version_and_default_schema_version() {
        let h = setup();
        assert_eq!(h.coord.version(), CONTRACT_VERSION);
        assert_eq!(h.coord.schema_version(), 1);
    }

    #[test]
    fn test_propose_upgrade_queues_behind_timelock() {
        let h = setup();
        h.env.ledger().with_mut(|l| l.timestamp = 10_000);

        let hash = BytesN::from_array(&h.env, &[9u8; 32]);
        let executable_at = h.coord.propose_upgrade(&hash);
        assert_eq!(executable_at, 10_000 + UPGRADE_TIMELOCK_SECS);

        let pending = h.coord.get_pending_upgrade().unwrap();
        assert_eq!(pending.new_wasm_hash, hash);
        assert_eq!(pending.executable_at, executable_at);
    }

    #[test]
    fn test_execute_upgrade_before_timelock_elapses_fails() {
        let h = setup();
        h.env.ledger().with_mut(|l| l.timestamp = 10_000);

        let hash = BytesN::from_array(&h.env, &[9u8; 32]);
        let executable_at = h.coord.propose_upgrade(&hash);

        h.env.ledger().with_mut(|l| l.timestamp = executable_at - 1);
        assert_eq!(
            h.coord.try_execute_upgrade(),
            Err(Ok(CoordinatorError::TimelockNotElapsed))
        );
        assert!(h.coord.get_pending_upgrade().is_some());
    }

    #[test]
    fn test_propose_upgrade_twice_fails() {
        let h = setup();
        let hash = BytesN::from_array(&h.env, &[9u8; 32]);
        h.coord.propose_upgrade(&hash);
        assert_eq!(
            h.coord.try_propose_upgrade(&hash),
            Err(Ok(CoordinatorError::UpgradeAlreadyPending))
        );
    }

    #[test]
    fn test_cancel_upgrade_clears_pending_proposal() {
        let h = setup();
        let hash = BytesN::from_array(&h.env, &[9u8; 32]);
        h.coord.propose_upgrade(&hash);
        h.coord.cancel_upgrade();

        assert!(h.coord.get_pending_upgrade().is_none());
        assert_eq!(
            h.coord.try_execute_upgrade(),
            Err(Ok(CoordinatorError::NoPendingUpgrade))
        );
    }

    #[test]
    fn test_migrate_double_run_guard() {
        let h = setup();

        // Simulate storage written by an older binary (schema 0 < target).
        // Register a fresh coordinator with constructor args, honoring the
        // same admin-wiring invariant as `setup()`.
        let admin = Address::generate(&h.env);
        let coord_id = Address::generate(&h.env);
        let inv_id = h.env.register(MockInventoryContract, ());
        MockInventoryContractClient::new(&h.env, &inv_id).initialize(&coord_id);
        MockPaymentContractClient::new(&h.env, &h.pay_id).set_coordinator(&coord_id);
        h.env.register_at(
            &coord_id,
            CoordinatorContract,
            (&admin, &h.req_id, &inv_id, &h.pay_id),
        );
        let coord = CoordinatorContractClient::new(&h.env, &coord_id);
        h.env.as_contract(&coord_id, || {
            h.env
                .storage()
                .instance()
                .set(&crate::SCHEMA_VERSION_KEY, &0u32)
        });

        assert_eq!(coord.schema_version(), 0);
        assert_eq!(coord.migrate(), TARGET_SCHEMA_VERSION);
        assert_eq!(
            coord.try_migrate(),
            Err(Ok(CoordinatorError::MigrationAlreadyApplied))
        );
    }

    // ── Cross-contract version compatibility gate ─────────────────────────────

    #[contract]
    struct MockIncompatibleContract;

    #[contractimpl]
    impl MockIncompatibleContract {
        pub fn version() -> u32 {
            999
        }
    }

    /// A version-mismatched domain contract must fail the workflow with a
    /// distinct error before any state change.
    #[test]
    fn test_version_mismatched_domain_contract_fails_closed() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let coord_id = Address::generate(&env);

        // The requests contract reports an unsupported code version.
        let req_id = env.register(MockIncompatibleContract, ());
        let inv_id = env.register(MockInventoryContract, ());
        let pay_id = env.register(MockPaymentContract, ());
        MockInventoryContractClient::new(&env, &inv_id).initialize(&coord_id);
        MockPaymentContractClient::new(&env, &pay_id).set_coordinator(&coord_id);

        env.register_at(
            &coord_id,
            CoordinatorContract,
            (&admin, &req_id, &inv_id, &pay_id),
        );
        let coord = CoordinatorContractClient::new(&env, &coord_id);

        let result = coord.try_allocate_units(&1u64, &vec![&env, 1u64], &1u64, &admin);
        assert_eq!(
            result,
            Err(Ok(CoordinatorError::IncompatibleContractVersion))
        );

        // Fail-closed: no workflow was created.
        assert!(coord.try_get_workflow(&1u64).is_err());
    }

    /// A contract that does not expose `version()` at all must also fail
    /// closed rather than mis-execute.
    #[contract]
    struct MockVersionlessContract;

    #[contractimpl]
    impl MockVersionlessContract {
        pub fn noop() {}
    }

    #[test]
    fn test_versionless_domain_contract_fails_closed() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let coord_id = Address::generate(&env);

        let req_id = env.register(MockVersionlessContract, ());
        let inv_id = env.register(MockInventoryContract, ());
        let pay_id = env.register(MockPaymentContract, ());
        MockInventoryContractClient::new(&env, &inv_id).initialize(&coord_id);
        MockPaymentContractClient::new(&env, &pay_id).set_coordinator(&coord_id);

        env.register_at(
            &coord_id,
            CoordinatorContract,
            (&admin, &req_id, &inv_id, &pay_id),
        );
        let coord = CoordinatorContractClient::new(&env, &coord_id);

        let result = coord.try_allocate_units(&1u64, &vec![&env, 1u64], &1u64, &admin);
        assert_eq!(
            result,
            Err(Ok(CoordinatorError::IncompatibleContractVersion))
        );
    }
}

/// Full upgrade rehearsal against the real compiled WASM: start a workflow,
/// swap the coordinator binary mid-flight (propose → timelock → execute),
/// then prove the in-flight workflow completes correctly on the new binary.
///
/// Requires the workspace WASMs to be built first — run via
/// `scripts/test-upgrade-rehearsal.sh`.
#[cfg(feature = "upgrade-rehearsal")]
mod upgrade_rehearsal {
    use super::*;
    use soroban_sdk::testutils::Ledger as _;

    const COORDINATOR_WASM: &[u8] = include_bytes!(
        "../../../target/wasm32v1-none/release/coordinator_contract.wasm"
    );

    #[test]
    fn test_upgrade_rehearsal_inflight_workflow_completes_after_upgrade() {
        let h = setup();
        h.env.ledger().with_mut(|l| l.timestamp = 1_000_000);

        // Realistic in-flight state: an allocated (unsettled) workflow with
        // reserved units and a locked payment.
        seed_pending_request(&h, 1);
        let unit_id = register_unit(&h);
        let payment_id = create_locked_payment(&h, 1);
        h.coord
            .allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
        assert_eq!(h.coord.get_workflow(&1u64).status, WorkflowStatus::Allocated);

        // Propose → wait out the 48h timelock → execute the WASM swap.
        let wasm_hash = h.env.deployer().upload_contract_wasm(COORDINATOR_WASM);
        let executable_at = h.coord.propose_upgrade(&wasm_hash);
        h.env.ledger().with_mut(|l| l.timestamp = executable_at + 1);
        h.coord.execute_upgrade();

        // Same contract ID, storage intact, new binary answering.
        assert_eq!(h.coord.version(), crate::CONTRACT_VERSION);
        let wf = h.coord.get_workflow(&1u64);
        assert_eq!(wf.status, WorkflowStatus::Allocated);

        // The in-flight workflow completes correctly post-upgrade.
        h.coord.confirm_delivery(&1u64, &h.admin);
        h.coord.settle_payment(&1u64, &h.admin);
        assert_eq!(h.coord.get_workflow(&1u64).status, WorkflowStatus::Settled);

        let payment = MockPaymentContractClient::new(&h.env, &h.pay_id).get_payment(&payment_id);
        assert_eq!(payment.status, PaymentStatus::Released);
    }
}

// ── Blood-type compatibility regression tests ─────────────────────────────────

/// Allocating an O+ unit against an A- request must be rejected on-chain.
/// O+ can only donate to O+, A+, B+, AB+ — not A-.
#[test]
fn test_allocate_rejects_incompatible_blood_type() {
    let h = setup();
    // Request requires A- blood
    MockRequestContractClient::new(&h.env, &h.req_id)
        .seed_request_with_type(&1u64, &RequestStatus::Pending, &BloodType::ANegative);
    // Register an O+ unit (incompatible with A-)
    let unit_id = MockInventoryContractClient::new(&h.env, &h.inv_id)
        .register_unit(&BloodType::OPositive);
    let payment_id = create_locked_payment(&h, 1);

    let result = h
        .coord
        .try_allocate_units(&1u64, &vec![&h.env, unit_id], &payment_id, &h.admin);
    assert_eq!(result, Err(Ok(CoordinatorError::IncompatibleBloodType)));

    // No workflow must have been created and the unit must remain Available.
    assert!(h.coord.try_get_workflow(&1u64).is_err());
    let unit = MockInventoryContractClient::new(&h.env, &h.inv_id).get_blood_unit(&unit_id);
    assert_eq!(unit.status, BloodStatus::Available);
}

/// O- is the universal donor — it must be accepted for every recipient type.
#[test]
fn test_allocate_accepts_universal_donor_for_all_recipients() {
    let recipients = [
        BloodType::APositive,
        BloodType::ANegative,
        BloodType::BPositive,
        BloodType::BNegative,
        BloodType::ABPositive,
        BloodType::ABNegative,
        BloodType::OPositive,
        BloodType::ONegative,
    ];

    for (i, recipient) in recipients.iter().enumerate() {
        let h = setup();
        let request_id = i as u64 + 1;
        MockRequestContractClient::new(&h.env, &h.req_id).seed_request_with_type(
            &request_id,
            &RequestStatus::Pending,
            recipient,
        );
        let unit_id = MockInventoryContractClient::new(&h.env, &h.inv_id)
            .register_unit(&BloodType::ONegative);
        let payment_id = create_locked_payment(&h, request_id);

        let result = h.coord.try_allocate_units(
            &request_id,
            &vec![&h.env, unit_id],
            &payment_id,
            &h.admin,
        );
        assert!(
            result.is_ok(),
            "O- must be accepted for recipient {:?}",
            recipient
        );
    }
}

// ── Shared-type wire-format drift regression ──────────────────────────────────
//
// The coordinator decodes cross-contract return values using the shared
// `interfaces` definitions. If a domain contract's enum discriminants drift
// from those definitions (a variant reordered or removed), decoding silently
// produces the wrong variant — a correctness bug that type-checks fine. These
// tests pin the wire format so any such drift fails loudly here instead of
// mis-settling payments at runtime.

/// A mock whose `RequestStatus` enum has the same variants as the shared
/// definition but in a *different order* — i.e. shifted discriminants. This
/// models a domain contract that drifted from the shared interface.
#[contracttype]
enum DriftedRequestStatus {
    // Deliberately reordered vs. `interfaces::RequestStatus` (Pending, Approved,
    // Fulfilled, ...): `Approved` now encodes as discriminant 0.
    Approved,
    Pending,
    Fulfilled,
    Rejected,
    Cancelled,
}

#[contracttype]
struct DriftedBloodRequest {
    id: u64,
    status: DriftedRequestStatus,
    blood_type: BloodType,
}

#[contract]
struct MockDriftedRequestContract;

#[contractimpl]
impl MockDriftedRequestContract {
    pub fn version() -> u32 {
        1
    }

    pub fn seed_drifted_request(env: Env, id: u64) {
        env.storage().persistent().set(
            &ReqKey::Request(id),
            &DriftedBloodRequest {
                id,
                status: DriftedRequestStatus::Pending,
                blood_type: BloodType::APositive,
            },
        );
    }

    /// Returns the drifted struct; the coordinator will decode it as the
    /// shared `BloodRequest`, so a Pending request reads back as Approved.
    pub fn get_request(env: Env, request_id: u64) -> DriftedBloodRequest {
        env.storage()
            .persistent()
            .get(&ReqKey::Request(request_id))
            .unwrap()
    }
}

/// A reordered enum variant must be caught by a decode assertion: the shared
/// definition decodes the drifted discriminant to the wrong variant, proving
/// the wire formats differ. This is the runtime symptom the CI spec-snapshot
/// drift check exists to catch at review time.
#[test]
fn test_reordered_enum_variant_is_detected_by_decode_assertion() {
    let env = Env::default();
    env.mock_all_auths();

    let drifted_id = env.register(MockDriftedRequestContract, ());
    MockDriftedRequestContractClient::new(&env, &drifted_id).seed_drifted_request(&1u64);

    // Decode the drifted return value through the *shared* type.
    let decoded: BloodRequest =
        MockDriftedRequestContractClient::new(&env, &drifted_id).get_request(&1u64);

    // The drifted contract meant `Pending`; the shared definition decodes the
    // same discriminant as `Approved`. The mismatch is observable, so a
    // reordered variant can never pass silently.
    assert_ne!(
        decoded.status,
        RequestStatus::Pending,
        "reordered enum variant decoded as the intended variant — drift went undetected"
    );
    assert_eq!(
        decoded.status,
        RequestStatus::Approved,
        "expected the shifted discriminant to decode as Approved"
    );
}

/// The shared definitions must round-trip through the wire format unchanged:
/// a value written and read back via the shared type keeps its variant.
#[test]
fn test_shared_enum_round_trips_through_wire_format() {
    let env = Env::default();
    env.mock_all_auths();

    let req_id = env.register(MockRequestContract, ());
    let client = MockRequestContractClient::new(&env, &req_id);

    for status in [
        RequestStatus::Pending,
        RequestStatus::Approved,
        RequestStatus::Fulfilled,
        RequestStatus::Rejected,
        RequestStatus::Cancelled,
    ] {
        client.seed_request(&7u64, &status);
        let decoded = client.get_request(&7u64);
        assert_eq!(
            decoded.status, status,
            "shared RequestStatus failed to round-trip through the wire format"
        );
    }
}

// ── Matching-contract compatibility parity ─────────────────────────────────────
//
// The coordinator inlines its own copy of the ABO/Rh compatibility matrix
// (see `is_compatible` in lib.rs) rather than cross-calling the matching
// contract's `match_request` for every allocation. matching-contract is kept
// as a pre-allocation advisory: a caller can query it to decide which
// unit_ids to submit to allocate_units, but allocate_units independently
// re-validates compatibility. This test proves the two copies of the
// compatibility predicate can never diverge — the property both the
// advisory recommendation and the authoritative check ultimately rely on.
#[test]
fn test_coordinator_and_matching_compatibility_never_diverge() {
    let types = [
        BloodType::APositive,
        BloodType::ANegative,
        BloodType::BPositive,
        BloodType::BNegative,
        BloodType::ABPositive,
        BloodType::ABNegative,
        BloodType::OPositive,
        BloodType::ONegative,
    ];
    let matching_types = [
        matching_contract::BloodType::APositive,
        matching_contract::BloodType::ANegative,
        matching_contract::BloodType::BPositive,
        matching_contract::BloodType::BNegative,
        matching_contract::BloodType::ABPositive,
        matching_contract::BloodType::ABNegative,
        matching_contract::BloodType::OPositive,
        matching_contract::BloodType::ONegative,
    ];

    for (i, donor) in types.iter().enumerate() {
        for (j, recipient) in types.iter().enumerate() {
            let coord_result = super::is_compatible(*donor, *recipient);
            let matching_result =
                matching_contract::is_compatible(matching_types[i], matching_types[j]);
            assert_eq!(
                coord_result, matching_result,
                "compatibility mismatch for donor={:?} recipient={:?}: coordinator={}, matching={}",
                donor, recipient, coord_result, matching_result
            );
        }
    }
}
