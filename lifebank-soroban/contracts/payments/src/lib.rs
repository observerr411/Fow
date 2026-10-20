#![no_std]
use soroban_sdk::token;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, String,
    Vec,
};

// Cross-contract types come from the shared `interfaces` crate — one
// definition, compiler-enforced. Do not re-declare them here.
use interfaces::{
    bump, CredentialType, DisputeReason, IdentityContractClient, Payment, PaymentStatus,
    StorageClass, TtlPolicy,
};

// ── Types ──────────────────────────────────────────────────────────────────────
//
// `PaymentStatus`, `DisputeReason`, and `Payment` are defined in the shared
// `interfaces` crate (see its docs for the lifecycle state machine and the
// append-only enum policy). Only payments-internal helpers live here.

fn dispute_reason_to_code(reason: DisputeReason) -> u32 {
    match reason {
        DisputeReason::FailedDelivery => 1,
        DisputeReason::TemperatureExcursion => 2,
        DisputeReason::PaymentContested => 3,
        DisputeReason::WrongItem => 4,
        DisputeReason::DamagedGoods => 5,
        DisputeReason::LateDelivery => 6,
        DisputeReason::Other => 7,
    }
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentStats {
    pub total_locked: i128,
    pub total_released: i128,
    pub total_refunded: i128,
    pub count_locked: u32,
    pub count_released: u32,
    pub count_refunded: u32,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentPage {
    pub items: Vec<Payment>,
    pub total: u64,
    pub page: u32,
    pub page_size: u32,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DonationPledge {
    pub id: u64,
    pub donor: Address,
    pub amount_per_period: i128,
    pub interval_secs: u64,
    pub payee_pool: String,
    pub cause: String,
    pub region: String,
    pub emergency_pool: bool,
    pub active: bool,
    pub created_at: u64,
}

/// On-chain vesting schedule for donor reward tokens.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VestingSchedule {
    pub donor: Address,
    pub total_amount: i128,
    pub cliff_timestamp: u64,
    pub vest_end_timestamp: u64,
    pub claimed: i128,
}

#[contracterror]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Error {
    PaymentNotFound = 500,
    InvalidAmount = 501,
    SamePayerPayee = 502,
    InvalidPage = 503,
    NotPledgeDonor = 504,
    InsufficientEscrowFunds = 505,
    Unauthorized = 506,
    /// The specific operation flag is paused (granular circuit-breaker).
    ContractPaused = 507,
    /// The requested pause flag does not exist.
    UnknownPauseFlag = 530,
    CliffNotReached = 508,
    VestingNotFound = 509,
    NothingToClaim = 510,
    /// A payment already exists for this request.
    DuplicatePayment = 511,
    /// The associated request is not in a state that permits payment.
    RequestNotPayable = 512,
    /// The request referenced by this payment does not exist.
    RequestNotFound = 513,
    /// Payment has no escrowed token — cannot release or refund funds.
    NotEscrowPayment = 514,
    /// Payment is not in the Locked state required for settlement.
    PaymentNotLocked = 515,
    /// Dispute timeout has not yet elapsed.
    DisputeNotExpired = 516,
    /// Storage is already at the schema version this binary targets.
    MigrationAlreadyApplied = 517,
    /// An upgrade proposal is already queued.
    UpgradeAlreadyPending = 518,
    /// No upgrade proposal is queued.
    NoPendingUpgrade = 519,
    /// The upgrade timelock window has not elapsed yet.
    TimelockNotElapsed = 520,
    /// A batch exceeds the maximum entries accepted in one transaction.
    BatchTooLarge = 521,
    /// `resolve_dispute` was called on a payment that is not Disputed.
    PaymentNotDisputed = 522,
    /// A persistent entry was accessed past its documented TTL policy
    /// horizon (see the storage-class table below). Distinct from a host
    /// panic so callers can surface a recoverable, documented failure.
    StorageHorizonExceeded = 523,
}

// ── Storage classes & TTL policy ──────────────────────────────────────────────
//
// Every persistent key in this contract has a documented storage class and
// TTL policy. `bump` (from `interfaces`) is invoked on every read/write path
// so entries never silently archive while still live.
//
// | Key family                         | Class      | Threshold | Extend to |
// |------------------------------------|------------|-----------|-----------|
// | `payment_key(id)`                  | Persistent | 30 days   | 90 days   |
// | `pledge_key(id)`                   | Persistent | 30 days   | 90 days   |
// | `vesting_key(donor)`               | Persistent | 30 days   | 90 days   |
// | payer/payee/status index pages     | Persistent | 30 days   | 90 days   |
// | index count keys                   | Persistent | 30 days   | 90 days   |
// | `status_position_key(id)`          | Persistent | 30 days   | 90 days   |
// | `request_payment_key(request_id)`  | Persistent | 30 days   | 90 days   |
// | instance storage (all keys)        | Instance   | 30 days   | 90 days   |
//
// Domain-deadline rule: when a payment is locked (escrow created), its entry
// is extended to at least the dispute-resolution horizon
// (`DISPUTE_RESOLUTION_HORIZON_SECS`) so a lock-then-dispute flow spanning
// the documented gap never archives the live escrow record.

/// Ledgers per day at ~5s close time.
pub const LEDGERS_PER_DAY: u32 = 17_280;
/// Dispute-resolution horizon: 30 days. Locked escrow entries are extended
/// to at least this far past the lock timestamp.
pub const DISPUTE_RESOLUTION_HORIZON_SECS: u64 = 30 * 24 * 3600;

/// TTL policy for payments persistent entries.
const PAYMENT_TTL: TtlPolicy = TtlPolicy {
    class: StorageClass::Persistent,
    threshold_ledgers: 30 * LEDGERS_PER_DAY,
    extend_to_ledgers: 90 * LEDGERS_PER_DAY,
};

/// TTL policy for instance storage.
const INSTANCE_TTL: TtlPolicy = TtlPolicy {
    class: StorageClass::Instance,
    threshold_ledgers: 30 * LEDGERS_PER_DAY,
    extend_to_ledgers: 90 * LEDGERS_PER_DAY,
};

/// Extend instance storage at entrypoint level.
fn bump_instance(env: &Env) {
    bump(env, &INSTANCE_TTL);
}

// ── Storage keys ───────────────────────────────────────────────────────────────

const PAYMENT_COUNTER: soroban_sdk::Symbol = symbol_short!("PAY_CTR");
const PLEDGE_COUNTER: soroban_sdk::Symbol = symbol_short!("PLG_CTR");
const ADMIN_KEY: soroban_sdk::Symbol = symbol_short!("ADMIN");
/// Guardian address — can pause any flag instantly (break-glass).
const GUARDIAN_KEY: soroban_sdk::Symbol = symbol_short!("GUARDIAN");
/// Coordinator contract address — additive to ADMIN_KEY, not a replacement.
/// Authorizes only `release_escrow`, `refund_escrow`, `record_dispute`, and
/// `resolve_dispute` (workflow settlement), leaving every other admin-gated
/// entry point (pause, dispute-timeout, vesting, upgrades) controlled solely
/// by the human/multisig admin even when a coordinator is wired in.
const COORDINATOR_KEY: soroban_sdk::Symbol = symbol_short!("COORD");
// ── Granular pause flags ───────────────────────────────────────────────────────
// Pausing `lock` must NOT block `release`, `refund`, or `dispute`.
// Each flag is stored as Option<u64> — None = not paused, Some(ts) = paused_at.
const PAUSE_LOCK: soroban_sdk::Symbol = symbol_short!("P_LOCK");
const PAUSE_RELEASE: soroban_sdk::Symbol = symbol_short!("P_REL");
const PAUSE_DISPUTE: soroban_sdk::Symbol = symbol_short!("P_DISP");
/// Maximum duration any flag may remain paused (30 days).
/// After this window refund/dispute paths auto-enable regardless of admin keys.
pub const MAX_PAUSE_SECS: u64 = 30 * 24 * 3600;
const REWARD_TOKEN_KEY: soroban_sdk::Symbol = symbol_short!("RWD_TOK");
/// Instance-level aggregate stats.
const STATS_KEY: soroban_sdk::Symbol = symbol_short!("STATS");
/// Instance storage key for the requests contract address (optional).
const REQ_CONTRACT: soroban_sdk::Symbol = symbol_short!("REQ_CTR");
/// Instance storage key for the identity contract address (optional).
const IDENTITY_CONTRACT: soroban_sdk::Symbol = symbol_short!("ID_CTR");
/// Default dispute auto-refund timeout in seconds (7 days).
const DEFAULT_DISPUTE_TIMEOUT_SECS: u64 = 7 * 24 * 3600;
/// Instance storage key for the dispute timeout override.
const DISPUTE_TIMEOUT: soroban_sdk::Symbol = symbol_short!("DISP_TO");
/// Maximum IDs stored in one enumeration index entry.
pub const INDEX_PAGE_SIZE: u32 = 100;
/// Maximum payments accepted by one batch call.
pub const MAX_BATCH_PAYMENTS: u32 = 100;
/// Maximum records returned by one pagination call.
pub const MAX_PAGE_SIZE: u32 = 100;

fn payment_key(id: u64) -> (u64, &'static str) {
    (id, "pay")
}

fn pledge_key(id: u64) -> (u64, &'static str) {
    (id, "plg")
}

fn payer_index_page_key(payer: &Address, page: u32) -> (Address, u32, &'static str) {
    (payer.clone(), page, "pip")
}

fn payee_index_page_key(payee: &Address, page: u32) -> (Address, u32, &'static str) {
    (payee.clone(), page, "pyp")
}

fn status_index_page_key(status: PaymentStatus, page: u32) -> (u32, u32, &'static str) {
    (status_index_code(status), page, "sip")
}

fn status_index_code(status: PaymentStatus) -> u32 {
    match status {
        PaymentStatus::Pending => 0,
        PaymentStatus::Locked => 1,
        PaymentStatus::Released => 2,
        PaymentStatus::Refunded => 3,
        PaymentStatus::Disputed => 4,
        PaymentStatus::Cancelled => 5,
    }
}

fn payer_index_count_key(payer: &Address) -> (Address, &'static str) {
    (payer.clone(), "pic")
}

fn payee_index_count_key(payee: &Address) -> (Address, &'static str) {
    (payee.clone(), "pyc")
}

fn status_index_count_key(status: PaymentStatus) -> (u32, &'static str) {
    (status_index_code(status), "sic")
}

fn status_position_key(payment_id: u64) -> (u64, &'static str) {
    (payment_id, "spos")
}

fn request_payment_key(request_id: u64) -> (u64, &'static str) {
    (request_id, "reqp")
}

fn normalized_page_size(page_size: u32) -> u32 {
    if page_size == 0 {
        20
    } else {
        page_size.min(MAX_PAGE_SIZE)
    }
}

fn get_counter(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&PAYMENT_COUNTER)
        .unwrap_or(0u64)
}

fn set_counter(env: &Env, val: u64) {
    env.storage().instance().set(&PAYMENT_COUNTER, &val);
}

fn get_pledge_counter(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&PLEDGE_COUNTER)
        .unwrap_or(0u64)
}

fn set_pledge_counter(env: &Env, val: u64) {
    env.storage().instance().set(&PLEDGE_COUNTER, &val);
}

fn store_payment(env: &Env, payment: &Payment) {
    let key = payment_key(payment.id);
    env.storage().persistent().set(&key, payment);
    bump(env, &PAYMENT_TTL);
}

fn load_payment(env: &Env, id: u64) -> Option<Payment> {
    let key = payment_key(id);
    let value = env.storage().persistent().get(&key);
    if value.is_some() {
        bump(env, &PAYMENT_TTL);
    }
    value
}

/// Extend a locked payment's TTL to at least the dispute-resolution horizon.
fn bump_payment_to_dispute_horizon(env: &Env) {
    bump(env, &PAYMENT_TTL);
    // The shared `bump` already extends to `extend_to_ledgers` (90 days),
    // which exceeds the 30-day dispute-resolution horizon. This helper exists
    // to make the domain-deadline rule explicit at the lock site.
    let _ = DISPUTE_RESOLUTION_HORIZON_SECS;
}

fn store_pledge(env: &Env, pledge: &DonationPledge) {
    let key = pledge_key(pledge.id);
    env.storage().persistent().set(&key, pledge);
    bump(env, &PAYMENT_TTL);
}

fn load_pledge(env: &Env, id: u64) -> Option<DonationPledge> {
    let key = pledge_key(id);
    let value = env.storage().persistent().get(&key);
    if value.is_some() {
        bump(env, &PAYMENT_TTL);
    }
    value
}

fn vesting_key(donor: &Address) -> (Address, &'static str) {
    (donor.clone(), "vest")
}

fn store_vesting(env: &Env, schedule: &VestingSchedule) {
    let key = vesting_key(&schedule.donor);
    env.storage().persistent().set(&key, schedule);
    bump(env, &PAYMENT_TTL);
}

fn load_vesting(env: &Env, donor: &Address) -> Option<VestingSchedule> {
    let key = vesting_key(donor);
    let value = env.storage().persistent().get(&key);
    if value.is_some() {
        bump(env, &PAYMENT_TTL);
    }
    value
}

// ── Index helpers ──────────────────────────────────────────────────────────────

fn index_by_payer(env: &Env, payer: &Address, id: u64) {
    let count_key = payer_index_count_key(payer);
    let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
    let page_key = payer_index_page_key(payer, count / INDEX_PAGE_SIZE);
    let mut ids: Vec<u64> = env
        .storage()
        .persistent()
        .get(&page_key)
        .unwrap_or(Vec::new(env));
    ids.push_back(id);
    env.storage().persistent().set(&page_key, &ids);
    env.storage().persistent().set(&count_key, &(count + 1));
    bump(env, &PAYMENT_TTL);
}

fn index_by_payee(env: &Env, payee: &Address, id: u64) {
    let count_key = payee_index_count_key(payee);
    let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
    let page_key = payee_index_page_key(payee, count / INDEX_PAGE_SIZE);
    let mut ids: Vec<u64> = env
        .storage()
        .persistent()
        .get(&page_key)
        .unwrap_or(Vec::new(env));
    ids.push_back(id);
    env.storage().persistent().set(&page_key, &ids);
    env.storage().persistent().set(&count_key, &(count + 1));
    bump(env, &PAYMENT_TTL);
}

fn index_by_status(env: &Env, status: PaymentStatus, id: u64) {
    let count_key = status_index_count_key(status);
    let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
    let page_key = status_index_page_key(status, count / INDEX_PAGE_SIZE);
    let mut ids: Vec<u64> = env
        .storage()
        .persistent()
        .get(&page_key)
        .unwrap_or(Vec::new(env));
    ids.push_back(id);
    env.storage().persistent().set(&page_key, &ids);
    env.storage()
        .persistent()
        .set(&status_position_key(id), &(count, status));
    env.storage().persistent().set(&count_key, &(count + 1));
    bump(env, &PAYMENT_TTL);
}

fn index_by_request(env: &Env, request_id: u64, payment_id: u64) {
    env.storage()
        .persistent()
        .set(&request_payment_key(request_id), &payment_id);
    bump(env, &PAYMENT_TTL);
}

/// Remove a status index entry with a bounded swap-with-last operation.
fn remove_from_status_index(env: &Env, status: PaymentStatus, id: u64) {
    let (position, stored_status): (u32, PaymentStatus) = env
        .storage()
        .persistent()
        .get(&status_position_key(id))
        .unwrap();
    if stored_status != status {
        return;
    }
    let count_key = status_index_count_key(status);
    let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
    if count == 0 {
        return;
    }
    let last_position = count - 1;
    let last_page_key = status_index_page_key(status, last_position / INDEX_PAGE_SIZE);
    let mut last_page: Vec<u64> = env
        .storage()
        .persistent()
        .get(&last_page_key)
        .unwrap_or(Vec::new(env));
    let last_id = last_page.get(last_page.len() - 1).unwrap();
    if position != last_position {
        let target_page_key = status_index_page_key(status, position / INDEX_PAGE_SIZE);
        let mut target_page: Vec<u64> = env
            .storage()
            .persistent()
            .get(&target_page_key)
            .unwrap();
        target_page.set(position % INDEX_PAGE_SIZE, last_id);
        env.storage().persistent().set(&target_page_key, &target_page);
        env.storage()
            .persistent()
            .set(&status_position_key(last_id), &(position, status));
    }
    last_page.pop_back();
    if last_page.len() == 0 {
        env.storage().persistent().remove(&last_page_key);
    } else {
        env.storage().persistent().set(&last_page_key, &last_page);
    }
    env.storage().persistent().set(&count_key, &last_position);
    env.storage().persistent().remove(&status_position_key(id));
    bump(env, &PAYMENT_TTL);
}

// ── Stats helpers ──────────────────────────────────────────────────────────────

fn load_stats(env: &Env) -> PaymentStats {
    env.storage()
        .instance()
        .get(&STATS_KEY)
        .unwrap_or(PaymentStats {
            total_locked: 0,
            total_released: 0,
            total_refunded: 0,
            count_locked: 0,
            count_released: 0,
            count_refunded: 0,
        })
}

fn store_stats(env: &Env, stats: &PaymentStats) {
    env.storage().instance().set(&STATS_KEY, stats);
    bump_instance(env);
}

fn update_stats_on_transition(env: &Env, amount: i128, old: PaymentStatus, new: PaymentStatus) {
    let mut stats = load_stats(env);
    match old {
        PaymentStatus::Locked => {
            stats.total_locked -= amount;
            stats.count_locked = stats.count_locked.saturating_sub(1);
        }
        PaymentStatus::Released => {
            stats.total_released -= amount;
            stats.count_released = stats.count_released.saturating_sub(1);
        }
        PaymentStatus::Refunded => {
            stats.total_refunded -= amount;
            stats.count_refunded = stats.count_refunded.saturating_sub(1);
        }
        _ => {}
    }
    match new {
        PaymentStatus::Locked => {
            stats.total_locked += amount;
            stats.count_locked += 1;
        }
        PaymentStatus::Released => {
            stats.total_released += amount;
            stats.count_released += 1;
        }
        PaymentStatus::Refunded => {
            stats.total_refunded += amount;
            stats.count_refunded += 1;
        }
        _ => {}
    }
    store_stats(env, &stats);
}

// ── Request-contract cross-contract interface ─────────────────────────────────
//
// `RequestStatus`, `BloodRequest`, and `RequestContractClient` are defined in
// the shared `interfaces` crate.

use interfaces::{RequestContractClient, RequestStatus as ReqStatus};

/// Returns Ok(()) if `request_id` exists and is in Pending or Approved status.
fn validate_request_payable(
    env: &Env,
    requests_contract: &Address,
    request_id: u64,
) -> Result<(), Error> {
    let client = RequestContractClient::new(env, requests_contract);
    let req = client
        .try_get_request(&request_id)
        .map_err(|_| Error::RequestNotFound)?
        .map_err(|_| Error::RequestNotFound)?;
    match req.status {
        ReqStatus::Pending | ReqStatus::Approved => Ok(()),
        _ => Err(Error::RequestNotPayable),
    }
}

/// Attempt to move the linked request to Cancelled via the requests contract.
/// Silently ignores failures (request may already be terminal or contract not configured).
fn try_cancel_request(env: &Env, requests_contract: &Address, request_id: u64) {
    let client = RequestContractClient::new(env, requests_contract);
    // Best-effort: ignore errors so the payment refund is never blocked.
    let _ = client.try_update_request_status(
        &env.current_contract_address(),
        &request_id,
        &ReqStatus::Cancelled,
    );
}

// ── Contract ───────────────────────────────────────────────────────────────────

#[contract]
pub struct PaymentContract;

#[contractimpl]
impl PaymentContract {
    /// Atomic constructor — deploy + init in a single transaction.
    ///
    /// Eliminates the front-running window that existed when deploy and
    /// initialize were separate calls.  The legacy `initialize` entry-point
    /// remains for tooling that is not yet constructor-aware but is a no-op
    /// once the constructor has run.
    pub fn __constructor(env: Env, admin: Address, requests_contract: Option<Address>) {
        bump_instance(&env);
        admin.require_auth();
        if env.storage().instance().has(&ADMIN_KEY) {
            panic!("already initialized");
        }
        env.storage().instance().set(&ADMIN_KEY, &admin);
        if let Some(rc) = requests_contract {
            env.storage().instance().set(&REQ_CONTRACT, &rc);
        }
        env.storage()
            .instance()
            .set(&SCHEMA_VERSION_KEY, &TARGET_SCHEMA_VERSION);
    }

    /// Legacy initialize — kept for tooling compatibility.
    /// Returns `Unauthorized` (reuses the closest existing error code) if the
    /// constructor already ran; otherwise performs first-time initialization.
    pub fn initialize(
        env: Env,
        admin: Address,
        requests_contract: Option<Address>,
    ) -> Result<(), Error> {
        bump_instance(&env);
        admin.require_auth();
        if env.storage().instance().has(&ADMIN_KEY) {
            return Err(Error::Unauthorized);
        }
        env.storage().instance().set(&ADMIN_KEY, &admin);
        if let Some(rc) = requests_contract {
            env.storage().instance().set(&REQ_CONTRACT, &rc);
        }
        Ok(())
    }

    /// Set the guardian address. Admin only.
    /// The guardian can pause any flag instantly (break-glass) but cannot unpause.
    pub fn set_guardian(env: Env, admin: Address, guardian: Address) -> Result<(), Error> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        env.storage().instance().set(&GUARDIAN_KEY, &guardian);
        env.events().publish(
            (symbol_short!("pause"), symbol_short!("guardian")),
            guardian,
        );
        Ok(())
    }

    /// Pause a specific operation flag. Guardian or Admin only.
    /// `flag`: symbol_short!("lock") | symbol_short!("release") | symbol_short!("dispute").
    /// Pausing "lock" does NOT block refunds or disputes — exit paths remain open.
    pub fn pause_flag(
        env: Env,
        caller: Address,
        flag: soroban_sdk::Symbol,
    ) -> Result<(), Error> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        let guardian: Option<Address> = env.storage().instance().get(&GUARDIAN_KEY);
        if caller != admin && guardian.as_ref() != Some(&caller) {
            return Err(Error::Unauthorized);
        }
        let key = Self::flag_storage_key(&flag)?;
        let now = env.ledger().timestamp();
        env.storage().instance().set(&key, &now);
        env.events().publish(
            (symbol_short!("pause"), symbol_short!("paused")),
            (flag, now),
        );
        Ok(())
    }

    /// Unpause a specific operation flag. Admin only.
    /// Unpausing "release" or "dispute" flags requires the timelock to have elapsed
    /// (flag must have been paused for at least UPGRADE_TIMELOCK_SECS).
    /// This asymmetry is deliberate: stopping is instant, resuming is deliberate.
    pub fn unpause_flag(
        env: Env,
        admin: Address,
        flag: soroban_sdk::Symbol,
    ) -> Result<(), Error> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        let key = Self::flag_storage_key(&flag)?;
        // Funds-critical flags require timelock before unpausing.
        let is_funds_critical =
            flag == symbol_short!("release") || flag == symbol_short!("dispute");
        if is_funds_critical {
            if let Some(paused_at) = env.storage().instance().get::<_, u64>(&key) {
                let now = env.ledger().timestamp();
                if now < paused_at + UPGRADE_TIMELOCK_SECS {
                    return Err(Error::TimelockNotElapsed);
                }
            }
        }
        env.storage().instance().remove(&key);
        env.events().publish(
            (symbol_short!("pause"), symbol_short!("unpaused")),
            flag,
        );
        Ok(())
    }

    /// Returns whether a specific flag is currently paused (within safety window).
    pub fn is_flag_paused(env: Env, flag: soroban_sdk::Symbol) -> bool {
        let key = match Self::flag_storage_key(&flag) {
            Ok(k) => k,
            Err(_) => return false,
        };
        Self::flag_is_paused_now(&env, &key)
    }

    /// Map flag Symbol to its storage key.
    fn flag_storage_key(flag: &soroban_sdk::Symbol) -> Result<soroban_sdk::Symbol, Error> {
        if *flag == symbol_short!("lock") {
            Ok(PAUSE_LOCK)
        } else if *flag == symbol_short!("release") {
            Ok(PAUSE_RELEASE)
        } else if *flag == symbol_short!("dispute") {
            Ok(PAUSE_DISPUTE)
        } else {
            Err(Error::UnknownPauseFlag)
        }
    }

    /// Returns true if the flag is paused AND within the MAX_PAUSE_SECS safety window.
    /// After MAX_PAUSE_SECS the flag auto-enables so users can always exit even if
    /// admin keys are lost.
    fn flag_is_paused_now(env: &Env, key: &soroban_sdk::Symbol) -> bool {
        match env.storage().instance().get::<_, u64>(key) {
            None => false,
            Some(paused_at) => {
                let now = env.ledger().timestamp();
                now < paused_at + MAX_PAUSE_SECS
            }
        }
    }

    fn require_lock_not_paused(env: &Env) -> Result<(), Error> {
        if Self::flag_is_paused_now(env, &PAUSE_LOCK) {
            return Err(Error::ContractPaused);
        }
        Ok(())
    }

    fn require_release_not_paused(env: &Env) -> Result<(), Error> {
        if Self::flag_is_paused_now(env, &PAUSE_RELEASE) {
            return Err(Error::ContractPaused);
        }
        Ok(())
    }

    fn require_dispute_not_paused(env: &Env) -> Result<(), Error> {
        if Self::flag_is_paused_now(env, &PAUSE_DISPUTE) {
            return Err(Error::ContractPaused);
        }
        Ok(())
    }

    /// Legacy single-flag pause — pauses the "lock" flag only.
    /// Refunds and disputes remain callable. Admin or Guardian only.
    pub fn pause(env: Env, admin: Address) -> Result<(), Error> {
        admin.require_auth();
        let stored: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        let guardian: Option<Address> = env.storage().instance().get(&GUARDIAN_KEY);
        if admin != stored && guardian.as_ref() != Some(&admin) {
            return Err(Error::Unauthorized);
        }
        let now = env.ledger().timestamp();
        env.storage().instance().set(&PAUSE_LOCK, &now);
        env.events().publish(
            (symbol_short!("pause"), symbol_short!("paused")),
            (symbol_short!("lock"), now),
        );
        Ok(())
    }

    /// Legacy single-flag unpause — unpauses the "lock" flag. Admin only.
    pub fn unpause(env: Env, admin: Address) -> Result<(), Error> {
        admin.require_auth();
        let stored: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        if admin != stored {
            return Err(Error::Unauthorized);
        }
        env.storage().instance().remove(&PAUSE_LOCK);
        env.events().publish(
            (symbol_short!("pause"), symbol_short!("unpaused")),
            symbol_short!("lock"),
        );
        Ok(())
    }

    /// Returns true if the "lock" flag is paused (legacy compat).
    pub fn is_paused(env: Env) -> bool {
        Self::flag_is_paused_now(&env, &PAUSE_LOCK)
    }

    fn require_not_paused(env: &Env) -> Result<(), Error> {
        Self::require_lock_not_paused(env)
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), Error> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        if *caller != stored {
            return Err(Error::Unauthorized);
        }
        Ok(())
    }

    /// Designate the coordinator contract allowed to call `release_escrow`,
    /// `refund_escrow`, `record_dispute`, and `resolve_dispute` on behalf of
    /// workflow-driven settlement. Admin only.
    pub fn set_coordinator(env: Env, admin: Address, coordinator: Address) -> Result<(), Error> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        env.storage().instance().set(&COORDINATOR_KEY, &coordinator);
        env.events()
            .publish((symbol_short!("coord"), symbol_short!("set")), coordinator);
        Ok(())
    }

    /// The currently wired coordinator contract, if any.
    pub fn get_coordinator(env: Env) -> Option<Address> {
        env.storage().instance().get(&COORDINATOR_KEY)
    }

    /// Caller must be either the admin or the wired coordinator contract.
    fn require_admin_or_coordinator(env: &Env, caller: &Address) -> Result<(), Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        if *caller == admin {
            return Ok(());
        }
        let coordinator: Option<Address> = env.storage().instance().get(&COORDINATOR_KEY);
        if coordinator.as_ref() == Some(caller) {
            return Ok(());
        }
        Err(Error::Unauthorized)
    }

    /// Caller must be the payment's payer, its payee, the admin, or the
    /// wired coordinator (e.g. an automated temperature-breach report).
    fn require_dispute_recorder(env: &Env, caller: &Address, payment: &Payment) -> Result<(), Error> {
        if *caller == payment.payer || *caller == payment.payee {
            return Ok(());
        }
        Self::require_admin_or_coordinator(env, caller)
    }

    pub fn create_payment(
        env: Env,
        request_id: u64,
        payer: Address,
        payee: Address,
        amount: i128,
    ) -> Result<u64, Error> {
        bump_instance(&env);
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        if payer == payee {
            return Err(Error::SamePayerPayee);
        }
        payer.require_auth();

        // Reject if a payment for this request already exists.
        if env
            .storage()
            .persistent()
            .has(&request_payment_key(request_id))
        {
            return Err(Error::DuplicatePayment);
        }

        // Validate request state if the requests contract is configured.
        if let Some(rc) = env.storage().instance().get::<_, Address>(&REQ_CONTRACT) {
            validate_request_payable(&env, &rc, request_id)?;
        }

        let id = get_counter(&env) + 1;
        set_counter(&env, id);

        let now = env.ledger().timestamp();
        let payment = Payment {
            id,
            request_id,
            payer: payer.clone(),
            payee: payee.clone(),
            amount,
            status: PaymentStatus::Pending,
            created_at: now,
            updated_at: now,
            dispute_reason_code: None,
            dispute_case_id: None,
            dispute_resolved: false,
            token: None,
        };

        store_payment(&env, &payment);
        index_by_payer(&env, &payer, id);
        index_by_payee(&env, &payee, id);
        index_by_status(&env, PaymentStatus::Pending, id);
        index_by_request(&env, request_id, id);

        env.events().publish(
            (
                symbol_short!("payment"),
                symbol_short!("created"),
                symbol_short!("v1"),
            ),
            id,
        );

        Ok(id)
    }

    /// Batch-create multiple payments in a single transaction.
    pub fn batch_create_payments(
        env: Env,
        payments: Vec<(u64, Address, Address, i128)>,
    ) -> Result<Vec<u64>, Error> {
        bump_instance(&env);
        Self::require_not_paused(&env)?;
        if payments.len() > MAX_BATCH_PAYMENTS {
            return Err(Error::BatchTooLarge);
        }
        let mut ids: Vec<u64> = Vec::new(&env);
        for i in 0..payments.len() {
            let (request_id, payer, payee, amount) = payments.get(i).unwrap();
            let id = Self::create_payment(env.clone(), request_id, payer, payee, amount)?;
            ids.push_back(id);
        }
        Ok(ids)
    }

    /// Create an escrow-backed payment: transfers `amount` of `token` from
    /// `hospital` into the contract immediately, locking the funds on-chain.
    pub fn create_escrow(
        env: Env,
        request_id: u64,
        hospital: Address,
        payee: Address,
        amount: i128,
        token: Address,
    ) -> Result<u64, Error> {
        bump_instance(&env);
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        if hospital == payee {
            return Err(Error::SamePayerPayee);
        }
        hospital.require_auth();

        // Reject if a payment for this request already exists.
        if env
            .storage()
            .persistent()
            .has(&request_payment_key(request_id))
        {
            return Err(Error::DuplicatePayment);
        }

        // Validate request state if the requests contract is configured.
        if let Some(rc) = env.storage().instance().get::<_, Address>(&REQ_CONTRACT) {
            validate_request_payable(&env, &rc, request_id)?;
        }

        let token_client = token::Client::new(&env, &token);
        let available = token_client.balance(&hospital);
        if available < amount {
            return Err(Error::InsufficientEscrowFunds);
        }
        token_client.transfer(&hospital, &env.current_contract_address(), &amount);

        let id = get_counter(&env) + 1;
        set_counter(&env, id);

        let now = env.ledger().timestamp();
        let payment = Payment {
            id,
            request_id,
            payer: hospital.clone(),
            payee: payee.clone(),
            amount,
            status: PaymentStatus::Locked,
            created_at: now,
            updated_at: now,
            dispute_reason_code: None,
            dispute_case_id: None,
            dispute_resolved: false,
            token: Some(token.clone()),
        };

        store_payment(&env, &payment);
        index_by_payer(&env, &hospital, id);
        index_by_payee(&env, &payee, id);
        index_by_status(&env, PaymentStatus::Locked, id);
        index_by_request(&env, request_id, id);
        update_stats_on_transition(&env, amount, PaymentStatus::Pending, PaymentStatus::Locked);
        // Domain-deadline rule: extend the locked payment entry's TTL to at
        // least the dispute-resolution horizon.
        bump_payment_to_dispute_horizon(&env);

        env.events().publish(
            (
                symbol_short!("payment"),
                symbol_short!("escrowed"),
                symbol_short!("v1"),
            ),
            id,
        );

        Ok(id)
    }

    /// Release escrowed funds to the payee. Admin only.
    /// Transfers the locked amount from the contract to the payee and marks
    /// the payment as Released.
    pub fn release_escrow(env: Env, caller: Address, payment_id: u64) -> Result<(), Error> {
        bump_instance(&env);
        caller.require_auth();
        Self::require_release_not_paused(&env)?;
        Self::require_admin_or_coordinator(&env, &caller)?;

        let mut payment = load_payment(&env, payment_id).ok_or(Error::PaymentNotFound)?;

        if payment.status != PaymentStatus::Locked {
            return Err(Error::PaymentNotLocked);
        }

        // Gate: payee must hold a valid BloodBankAccreditation at settlement time.
        if let Some(identity_addr) = env
            .storage()
            .instance()
            .get::<soroban_sdk::Symbol, Address>(&IDENTITY_CONTRACT)
        {
            let id_client = IdentityContractClient::new(&env, &identity_addr);
            // Request grace for an in-flight workflow; identity policy decides.
            let credentialed = id_client
                .try_is_valid(
                    &payment.payee,
                    &CredentialType::BloodBankAccreditation,
                    &true,
                )
                .unwrap_or(Ok(false))
                .unwrap_or(false);
            if !credentialed {
                return Err(Error::Unauthorized);
            }
        }

        let token_addr = payment.token.clone().ok_or(Error::NotEscrowPayment)?;
        let token_client = token::Client::new(&env, &token_addr);
        token_client.transfer(
            &env.current_contract_address(),
            &payment.payee,
            &payment.amount,
        );

        let old_status = payment.status;
        payment.status = PaymentStatus::Released;
        payment.updated_at = env.ledger().timestamp();
        store_payment(&env, &payment);

        remove_from_status_index(&env, old_status, payment_id);
        index_by_status(&env, PaymentStatus::Released, payment_id);
        update_stats_on_transition(&env, payment.amount, old_status, PaymentStatus::Released);

        env.events().publish(
            (symbol_short!("payment"), symbol_short!("released")),
            (payment_id, payment.payee.clone(), payment.amount),
        );
        Ok(())
    }

    /// Refund escrowed funds to the payer. Admin only.
    /// Transfers the locked amount from the contract back to the payer and
    /// marks the payment as Refunded.
    /// NOTE: refund is intentionally NOT gated by the lock flag — users must
    /// always be able to exit even when new escrows are paused.
    pub fn refund_escrow(env: Env, caller: Address, payment_id: u64) -> Result<(), Error> {
        bump_instance(&env);
        caller.require_auth();
        // Refund path is only blocked by the release flag (funds-critical),
        // and only while within the MAX_PAUSE_SECS safety window.
        Self::require_release_not_paused(&env)?;
        Self::require_admin_or_coordinator(&env, &caller)?;

        let mut payment = load_payment(&env, payment_id).ok_or(Error::PaymentNotFound)?;

        if payment.status != PaymentStatus::Locked {
            return Err(Error::PaymentNotLocked);
        }

        let token_addr = payment.token.clone().ok_or(Error::NotEscrowPayment)?;
        let token_client = token::Client::new(&env, &token_addr);
        token_client.transfer(
            &env.current_contract_address(),
            &payment.payer,
            &payment.amount,
        );

        let old_status = payment.status;
        payment.status = PaymentStatus::Refunded;
        payment.updated_at = env.ledger().timestamp();
        store_payment(&env, &payment);

        remove_from_status_index(&env, old_status, payment_id);
        index_by_status(&env, PaymentStatus::Refunded, payment_id);
        update_stats_on_transition(&env, payment.amount, old_status, PaymentStatus::Refunded);

        env.events().publish(
            (symbol_short!("payment"), symbol_short!("refunded")),
            (payment_id, payment.payer.clone(), payment.amount),
        );
        Ok(())
    }

    pub fn record_dispute(
        env: Env,
        caller: Address,
        payment_id: u64,
        reason: DisputeReason,
        case_id: String,
    ) -> Result<(), Error> {
        bump_instance(&env);
        caller.require_auth();
        // Dispute path is gated by its own flag, NOT the lock flag.
        // Users must always be able to raise a dispute on locked funds.
        Self::require_dispute_not_paused(&env)?;
        let mut payment = load_payment(&env, payment_id).ok_or(Error::PaymentNotFound)?;
        if payment.status != PaymentStatus::Locked {
            return Err(Error::PaymentNotLocked);
        }
        Self::require_dispute_recorder(&env, &caller, &payment)?;
        let old_status = payment.status;
        payment.status = PaymentStatus::Disputed;
        payment.dispute_reason_code = Some(dispute_reason_to_code(reason));
        payment.dispute_case_id = Some(case_id.clone());
        payment.dispute_resolved = false;
        payment.updated_at = env.ledger().timestamp();
        store_payment(&env, &payment);
        remove_from_status_index(&env, old_status, payment_id);
        index_by_status(&env, PaymentStatus::Disputed, payment_id);
        update_stats_on_transition(&env, payment.amount, old_status, PaymentStatus::Disputed);
        env.events().publish(
            (
                symbol_short!("payment"),
                symbol_short!("disputed"),
                symbol_short!("v1"),
            ),
            (payment_id, dispute_reason_to_code(reason), case_id),
        );
        Ok(())
    }

    pub fn resolve_dispute(env: Env, caller: Address, payment_id: u64) -> Result<(), Error> {
        bump_instance(&env);
        caller.require_auth();
        Self::require_dispute_not_paused(&env)?;
        Self::require_admin_or_coordinator(&env, &caller)?;
        let mut payment = load_payment(&env, payment_id).ok_or(Error::PaymentNotFound)?;
        if payment.status != PaymentStatus::Disputed {
            return Err(Error::PaymentNotDisputed);
        }
        payment.dispute_resolved = true;
        payment.status = PaymentStatus::Locked;
        payment.updated_at = env.ledger().timestamp();
        store_payment(&env, &payment);
        remove_from_status_index(&env, PaymentStatus::Disputed, payment_id);
        index_by_status(&env, PaymentStatus::Locked, payment_id);
        update_stats_on_transition(&env, payment.amount, PaymentStatus::Disputed, PaymentStatus::Locked);
        env.events().publish(
            (
                symbol_short!("payment"),
                symbol_short!("resolved"),
                symbol_short!("v1"),
            ),
            payment_id,
        );
        Ok(())
    }

    // ── Query functions ────────────────────────────────────────────────────────

    pub fn get_payment(env: Env, payment_id: u64) -> Result<Payment, Error> {
        bump_instance(&env);
        load_payment(&env, payment_id).ok_or(Error::PaymentNotFound)
    }

    pub fn get_payment_by_request(env: Env, request_id: u64) -> Result<Payment, Error> {
        bump_instance(&env);
        let key = request_payment_key(request_id);
        let payment_id: u64 = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::PaymentNotFound)?;
        bump(&env, &PAYMENT_TTL);
        load_payment(&env, payment_id).ok_or(Error::PaymentNotFound)
    }

    pub fn get_payments_by_payer(
        env: Env,
        payer: Address,
        page: u32,
        page_size: u32,
    ) -> PaymentPage {
        bump_instance(&env);
        let page_size = normalized_page_size(page_size);
        let total: u32 = env
            .storage()
            .persistent()
            .get(&payer_index_count_key(&payer))
            .unwrap_or(0);
        let start = (page as u64) * (page_size as u64);
        let index_page = (start / INDEX_PAGE_SIZE as u64) as u32;
        let offset = (start % INDEX_PAGE_SIZE as u64) as u32;
        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&payer_index_page_key(&payer, index_page))
            .unwrap_or(Vec::new(&env));
        Self::load_page(&env, ids, total as u64, offset, page, page_size)
    }

    pub fn get_payments_by_payee(
        env: Env,
        payee: Address,
        page: u32,
        page_size: u32,
    ) -> PaymentPage {
        bump_instance(&env);
        let page_size = normalized_page_size(page_size);
        let total: u32 = env
            .storage()
            .persistent()
            .get(&payee_index_count_key(&payee))
            .unwrap_or(0);
        let start = (page as u64) * (page_size as u64);
        let index_page = (start / INDEX_PAGE_SIZE as u64) as u32;
        let offset = (start % INDEX_PAGE_SIZE as u64) as u32;
        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&payee_index_page_key(&payee, index_page))
            .unwrap_or(Vec::new(&env));
        Self::load_page(&env, ids, total as u64, offset, page, page_size)
    }

    pub fn get_payments_by_status(
        env: Env,
        status: PaymentStatus,
        page: u32,
        page_size: u32,
    ) -> PaymentPage {
        bump_instance(&env);
        let page_size = normalized_page_size(page_size);
        let total: u32 = env
            .storage()
            .persistent()
            .get(&status_index_count_key(status))
            .unwrap_or(0);
        let start = (page as u64) * (page_size as u64);
        let index_page = (start / INDEX_PAGE_SIZE as u64) as u32;
        let offset = (start % INDEX_PAGE_SIZE as u64) as u32;
        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&status_index_page_key(status, index_page))
            .unwrap_or(Vec::new(&env));
        Self::load_page(&env, ids, total as u64, offset, page, page_size)
    }

    pub fn get_payment_statistics(env: Env) -> PaymentStats {
        bump_instance(&env);
        load_stats(&env)
    }

    pub fn get_payment_timeline(env: Env, page: u32, page_size: u32) -> PaymentPage {
        bump_instance(&env);
        let page_size = normalized_page_size(page_size);
        let total = get_counter(&env);
        let start = (page as u64) * (page_size as u64);
        let end = (start + page_size as u64).min(total);
        let mut items: Vec<Payment> = Vec::new(&env);
        if start < total {
            for i in start..end {
                if let Some(payment) = load_payment(&env, i + 1) {
                    items.push_back(payment);
                }
            }
        }

        PaymentPage {
            items,
            total,
            page,
            page_size,
        }
    }

    pub fn get_payment_count(env: Env) -> u64 {
        bump_instance(&env);
        get_counter(&env)
    }

    pub fn create_pledge(
        env: Env,
        donor: Address,
        amount_per_period: i128,
        interval_secs: u64,
        payee_pool: String,
        cause: String,
        region: String,
        emergency_pool: bool,
    ) -> Result<u64, Error> {
        bump_instance(&env);
        Self::require_not_paused(&env)?;
        donor.require_auth();
        if amount_per_period <= 0 {
            return Err(Error::InvalidAmount);
        }
        if interval_secs == 0 {
            return Err(Error::InvalidAmount);
        }

        let id = get_pledge_counter(&env) + 1;
        set_pledge_counter(&env, id);

        let pledge = DonationPledge {
            id,
            donor: donor.clone(),
            amount_per_period,
            interval_secs,
            payee_pool,
            cause,
            region,
            emergency_pool,
            active: true,
            created_at: env.ledger().timestamp(),
        };
        store_pledge(&env, &pledge);

        env.events().publish(
            (
                symbol_short!("pledge"),
                symbol_short!("create"),
                symbol_short!("v1"),
            ),
            id,
        );

        Ok(id)
    }

    pub fn get_pledge(env: Env, pledge_id: u64) -> Result<DonationPledge, Error> {
        bump_instance(&env);
        load_pledge(&env, pledge_id).ok_or(Error::PaymentNotFound)
    }

    pub fn set_pledge_active(
        env: Env,
        pledge_id: u64,
        donor: Address,
        active: bool,
    ) -> Result<(), Error> {
        bump_instance(&env);
        Self::require_not_paused(&env)?;
        donor.require_auth();
        let mut p = load_pledge(&env, pledge_id).ok_or(Error::PaymentNotFound)?;
        if p.donor != donor {
            return Err(Error::NotPledgeDonor);
        }
        p.active = active;
        store_pledge(&env, &p);
        Ok(())
    }

    // ── Vesting ────────────────────────────────────────────────────────────────

    pub fn create_vesting(
        env: Env,
        admin: Address,
        donor: Address,
        total_amount: i128,
        cliff_secs: u64,
        duration_secs: u64,
    ) -> Result<(), Error> {
        bump_instance(&env);
        admin.require_auth();
        Self::require_not_paused(&env)?;

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        if admin != stored_admin {
            return Err(Error::Unauthorized);
        }

        if total_amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        if duration_secs == 0 {
            return Err(Error::InvalidAmount);
        }

        let now = env.ledger().timestamp();
        let schedule = VestingSchedule {
            donor: donor.clone(),
            total_amount,
            cliff_timestamp: now + cliff_secs,
            vest_end_timestamp: now + duration_secs,
            claimed: 0,
        };

        store_vesting(&env, &schedule);

        env.events().publish(
            (symbol_short!("vest"), symbol_short!("created")),
            (donor, total_amount, now + cliff_secs, now + duration_secs),
        );

        Ok(())
    }

    pub fn claim_vested(env: Env, donor: Address, reward_token: Address) -> Result<i128, Error> {
        bump_instance(&env);
        donor.require_auth();
        Self::require_not_paused(&env)?;

        let mut schedule = load_vesting(&env, &donor).ok_or(Error::VestingNotFound)?;

        let now = env.ledger().timestamp();

        if now < schedule.cliff_timestamp {
            return Err(Error::CliffNotReached);
        }

        let vested = if now >= schedule.vest_end_timestamp {
            schedule.total_amount
        } else {
            let elapsed = now - schedule.cliff_timestamp;
            let duration = schedule.vest_end_timestamp - schedule.cliff_timestamp;
            (schedule.total_amount * elapsed as i128) / duration as i128
        };

        let claimable = vested - schedule.claimed;
        if claimable <= 0 {
            return Err(Error::NothingToClaim);
        }

        let new_claimed = schedule.claimed + claimable;
        if new_claimed > schedule.total_amount {
            return Err(Error::NothingToClaim);
        }

        schedule.claimed = new_claimed;
        store_vesting(&env, &schedule);

        let token_client = token::Client::new(&env, &reward_token);
        token_client.transfer(&env.current_contract_address(), &donor, &claimable);

        env.events().publish(
            (symbol_short!("vest"), symbol_short!("claimed")),
            (donor, claimable, new_claimed),
        );

        Ok(claimable)
    }

    pub fn get_vesting(env: Env, donor: Address) -> Result<VestingSchedule, Error> {
        bump_instance(&env);
        load_vesting(&env, &donor).ok_or(Error::VestingNotFound)
    }

    // ── Dispute timeout (#595) ─────────────────────────────────────────────────

    /// Override the dispute auto-refund timeout. Admin only.
    pub fn set_dispute_timeout(env: Env, admin: Address, timeout_secs: u64) -> Result<(), Error> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DISPUTE_TIMEOUT, &timeout_secs);
        Ok(())
    }

    /// Refund all Disputed+escrowed payments whose dispute has exceeded the
    /// timeout window, cancel the linked request, and emit events so off-chain
    /// projections can reconcile request state. Admin only.
    pub fn process_expired_disputes(
        env: Env,
        admin: Address,
        payment_ids: Vec<u64>,
    ) -> Result<Vec<u64>, Error> {
        bump_instance(&env);
        admin.require_auth();
        Self::require_not_paused(&env)?;
        Self::require_admin(&env, &admin)?;

        let timeout: u64 = env
            .storage()
            .instance()
            .get(&DISPUTE_TIMEOUT)
            .unwrap_or(DEFAULT_DISPUTE_TIMEOUT_SECS);
        let now = env.ledger().timestamp();
        let req_contract: Option<Address> =
            env.storage().instance().get::<_, Address>(&REQ_CONTRACT);

        let mut refunded: Vec<u64> = Vec::new(&env);

        for i in 0..payment_ids.len() {
            let pid = payment_ids.get(i).unwrap();
            let mut payment = match load_payment(&env, pid) {
                Some(p) => p,
                None => continue,
            };
            if payment.status != PaymentStatus::Disputed {
                continue;
            }
            if payment.token.is_none() {
                continue;
            }
            if now < payment.updated_at + timeout {
                continue;
            }

            let token_client = token::Client::new(&env, payment.token.as_ref().unwrap());
            token_client.transfer(
                &env.current_contract_address(),
                &payment.payer,
                &payment.amount,
            );

            let old_status = payment.status;
            payment.status = PaymentStatus::Refunded;
            payment.updated_at = now;
            store_payment(&env, &payment);
            remove_from_status_index(&env, old_status, pid);
            index_by_status(&env, PaymentStatus::Refunded, pid);
            update_stats_on_transition(&env, payment.amount, old_status, PaymentStatus::Refunded);

            if let Some(ref rc) = req_contract {
                try_cancel_request(&env, rc, payment.request_id);
            }

            env.events().publish(
                (symbol_short!("payment"), symbol_short!("refunded")),
                (pid, payment.payer.clone(), payment.amount),
            );
            // Request-level event for off-chain projections.
            env.events().publish(
                (symbol_short!("request"), symbol_short!("cancelled")),
                (payment.request_id, pid, now),
            );

            refunded.push_back(pid);
        }

        Ok(refunded)
    }

    // ── Internal helpers ───────────────────────────────────────────────────────

    fn load_page(
        env: &Env,
        ids: Vec<u64>,
        total: u64,
        offset: u32,
        page: u32,
        page_size: u32,
    ) -> PaymentPage {
        bump_instance(env);
        let mut items: Vec<Payment> = Vec::new(env);

        for i in offset..ids.len().min(offset + page_size) {
            let id = ids.get(i).unwrap();
            if let Some(p) = load_payment(env, id) {
                items.push_back(p);
            }
        }

        PaymentPage {
            items,
            total,
            page,
            page_size,
        }
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
    ) -> Result<u64, Error> {
        bump_instance(&env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        admin.require_auth();
        if env.storage().instance().has(&PENDING_UPGRADE_KEY) {
            return Err(Error::UpgradeAlreadyPending);
        }
        let now = env.ledger().timestamp();
        let pending = PendingUpgrade {
            new_wasm_hash,
            proposed_at: now,
            executable_at: now + UPGRADE_TIMELOCK_SECS,
        };
        env.storage().instance().set(&PENDING_UPGRADE_KEY, &pending);
        env.events().publish(
            (symbol_short!("upgrade"), symbol_short!("proposed")),
            pending.executable_at,
        );
        Ok(pending.executable_at)
    }

    /// Cancel the pending upgrade proposal. Admin only.
    pub fn cancel_upgrade(env: Env) -> Result<(), Error> {
        bump_instance(&env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        admin.require_auth();
        if !env.storage().instance().has(&PENDING_UPGRADE_KEY) {
            return Err(Error::NoPendingUpgrade);
        }
        env.storage().instance().remove(&PENDING_UPGRADE_KEY);
        env.events()
            .publish((symbol_short!("upgrade"), symbol_short!("canceled")), ());
        Ok(())
    }

    /// The currently pending upgrade proposal, if any.
    pub fn get_pending_upgrade(env: Env) -> Option<PendingUpgrade> {
        env.storage().instance().get(&PENDING_UPGRADE_KEY)
    }

    /// Execute the proposed upgrade once its timelock has elapsed. Admin
    /// only. The contract ID and all storage are preserved; call `migrate`
    /// afterwards when the new binary bumps `TARGET_SCHEMA_VERSION`.
    pub fn execute_upgrade(env: Env) -> Result<(), Error> {
        bump_instance(&env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        admin.require_auth();
        let pending: PendingUpgrade = env
            .storage()
            .instance()
            .get(&PENDING_UPGRADE_KEY)
            .ok_or(Error::NoPendingUpgrade)?;
        if env.ledger().timestamp() < pending.executable_at {
            return Err(Error::TimelockNotElapsed);
        }
        env.storage().instance().remove(&PENDING_UPGRADE_KEY);
        env.events()
            .publish((symbol_short!("upgrade"), symbol_short!("executed")), ());
        env.deployer().update_current_contract_wasm(pending.new_wasm_hash);
        Ok(())
    }

    /// Apply version-gated storage migrations after an upgrade. Admin only.
    /// Refuses to run once storage already sits at `TARGET_SCHEMA_VERSION`,
    /// so a migration can never be applied twice.
    pub fn migrate(env: Env) -> Result<u32, Error> {
        bump_instance(&env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .ok_or(Error::Unauthorized)?;
        admin.require_auth();
        let current = Self::schema_version(env.clone());
        if current >= TARGET_SCHEMA_VERSION {
            return Err(Error::MigrationAlreadyApplied);
        }
        // Version-gated transformations run here as the schema evolves, e.g.
        // `if current < 2 { /* rewrite v1 entries into the v2 layout */ }`.
        env.storage()
            .instance()
            .set(&SCHEMA_VERSION_KEY, &TARGET_SCHEMA_VERSION);
        Ok(TARGET_SCHEMA_VERSION)
    }
}

mod test;

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
