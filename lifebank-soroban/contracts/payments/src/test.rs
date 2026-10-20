#![cfg(test)]

use super::*;
use soroban_sdk::{testutils::Address as _, testutils::Ledger as _, Address, Env, Vec};

fn setup() -> (Env, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(PaymentContract, ());
    (env, contract_id)
}

fn make_payment(
    env: &Env,
    client: &PaymentContractClient,
    request_id: u64,
    amount: i128,
) -> (u64, Address, Address) {
    let payer = Address::generate(env);
    let payee = Address::generate(env);
    let id = client.create_payment(&request_id, &payer, &payee, &amount);
    (id, payer, payee)
}

/// Deploy a minimal Soroban token contract and mint `amount` to `recipient`.
fn deploy_token_with_balance(env: &Env, admin: &Address, recipient: &Address, amount: i128) -> Address {
    let token = env.register_stellar_asset_contract_v2(admin.clone());
    let token_id = token.address();
    let token_admin = soroban_sdk::token::StellarAssetClient::new(env, &token_id);
    token_admin.mint(recipient, &amount);
    token_id
}

/// Test-only: force a payment's status directly via the contract's own
/// internal storage helpers, bypassing all authorization and token-transfer
/// semantics. Used only to set up preconditions for tests that exercise
/// something other than the status-transition logic itself (pagination,
/// status indexing, aggregate stats). Mirrors the body of the removed
/// `update_status` entry point — production code deliberately no longer
/// exposes an unauthenticated status setter (see the state-machine doc
/// comment on `PaymentStatus` in `lib.rs`).
fn force_status(env: &Env, cid: &Address, id: u64, status: PaymentStatus) {
    env.as_contract(cid, || {
        let mut payment = load_payment(env, id).unwrap();
        let old_status = payment.status;
        payment.status = status;
        payment.updated_at = env.ledger().timestamp();
        store_payment(env, &payment);
        remove_from_status_index(env, old_status, id);
        index_by_status(env, status, id);
        update_stats_on_transition(env, payment.amount, old_status, status);
    });
}

// ── create_payment ─────────────────────────────────────────────────────────────

#[test]
fn test_create_payment_increments_counter() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let (id1, _, _) = make_payment(&env, &client, 1, 1000);
    let (id2, _, _) = make_payment(&env, &client, 2, 2000);
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    assert_eq!(client.get_payment_count(), 2);
}

#[test]
fn test_create_payment_rejects_zero_amount() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let payer = Address::generate(&env);
    let payee = Address::generate(&env);
    let result = client.try_create_payment(&1u64, &payer, &payee, &0i128);
    assert!(result.is_err());
}

#[test]
fn test_create_payment_rejects_negative_amount() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let payer = Address::generate(&env);
    let payee = Address::generate(&env);
    let result = client.try_create_payment(&1u64, &payer, &payee, &-100i128);
    assert!(result.is_err());
}

#[test]
fn test_create_payment_rejects_same_payer_payee() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let addr = Address::generate(&env);
    let result = client.try_create_payment(&1u64, &addr, &addr, &1000i128);
    assert!(result.is_err());
}

#[test]
fn test_create_payment_stores_correct_fields() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    env.ledger().with_mut(|l| l.timestamp = 5000);
    let payer = Address::generate(&env);
    let payee = Address::generate(&env);
    let id = client.create_payment(&42u64, &payer, &payee, &999i128);

    let p = client.get_payment(&id);
    assert_eq!(p.request_id, 42);
    assert_eq!(p.payer, payer);
    assert_eq!(p.payee, payee);
    assert_eq!(p.amount, 999);
    assert_eq!(p.status, PaymentStatus::Pending);
    assert_eq!(p.created_at, 5000);
}

// ── get_payment ────────────────────────────────────────────────────────────────

#[test]
fn test_get_payment_returns_not_found_for_missing_id() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let result = client.try_get_payment(&999u64);
    assert!(result.is_err());
}

#[test]
fn test_get_payment_returns_correct_payment() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let (id, payer, payee) = make_payment(&env, &client, 10, 500);
    let p = client.get_payment(&id);
    assert_eq!(p.id, id);
    assert_eq!(p.payer, payer);
    assert_eq!(p.payee, payee);
    assert_eq!(p.amount, 500);
}

// ── get_payment_by_request ─────────────────────────────────────────────────────

#[test]
fn test_get_payment_by_request_finds_correct_payment() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    make_payment(&env, &client, 1, 100);
    let (id2, _, _) = make_payment(&env, &client, 99, 200);
    make_payment(&env, &client, 3, 300);

    let p = client.get_payment_by_request(&99u64);
    assert_eq!(p.id, id2);
    assert_eq!(p.request_id, 99);
}

#[test]
fn test_get_payment_by_request_returns_not_found() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    make_payment(&env, &client, 1, 100);
    let result = client.try_get_payment_by_request(&999u64);
    assert!(result.is_err());
}

// ── duplicate-payment prevention (#599) ───────────────────────────────────────

#[test]
fn test_create_payment_rejects_duplicate_request_id() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    // First payment for request 42 succeeds.
    make_payment(&env, &client, 42, 500);
    // Second payment for the same request must be rejected.
    let payer = Address::generate(&env);
    let payee = Address::generate(&env);
    let result = client.try_create_payment(&42u64, &payer, &payee, &500i128);
    assert_eq!(result, Err(Ok(Error::DuplicatePayment)));
}

#[test]
fn test_create_escrow_rejects_duplicate_request_id() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let cid = env.register(PaymentContract, (&admin, &None::<Address>));
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 10_000);

    // First escrow for request 7 succeeds.
    client.create_escrow(&7u64, &hospital, &payee, &1_000i128, &token_id);

    // Second escrow for the same request must be rejected.
    let result = client.try_create_escrow(&7u64, &hospital, &payee, &500i128, &token_id);
    assert_eq!(result, Err(Ok(Error::DuplicatePayment)));
}

#[test]
fn test_get_payment_by_request_resolves_without_full_scan() {
    // Verify the index lookup returns the correct payment even when many
    // payments exist for other request IDs.
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    for i in 1u64..=20 {
        make_payment(&env, &client, i, 100);
    }
    let target_request_id = 13u64;
    let p = client.get_payment_by_request(&target_request_id);
    assert_eq!(p.request_id, target_request_id);
}

#[test]
fn test_terminal_payment_does_not_block_new_active_payment_for_different_request() {
    // Payments for distinct request IDs must never interfere.
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let (id1, _, _) = make_payment(&env, &client, 100, 200);
    force_status(&env, &cid, id1, PaymentStatus::Refunded);

    // A payment for a different request must still be accepted.
    let (id2, _, _) = make_payment(&env, &client, 101, 300);
    assert!(id2 > id1);
    let p = client.get_payment_by_request(&101u64);
    assert_eq!(p.id, id2);
}

// ── get_payments_by_payer ──────────────────────────────────────────────────────

#[test]
fn test_get_payments_by_payer_returns_only_payer_payments() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let payer_a = Address::generate(&env);
    let payee = Address::generate(&env);

    client.create_payment(&1u64, &payer_a, &payee, &100i128);
    client.create_payment(&2u64, &payer_a, &payee, &200i128);
    make_payment(&env, &client, 3, 300);

    let page = client.get_payments_by_payer(&payer_a, &0u32, &20u32);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.total, 2);
}

#[test]
fn test_get_payments_by_payer_empty_result() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let stranger = Address::generate(&env);
    let page = client.get_payments_by_payer(&stranger, &0u32, &20u32);
    assert_eq!(page.items.len(), 0);
    assert_eq!(page.total, 0);
}

#[test]
fn test_get_payments_by_payer_pagination() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let payer = Address::generate(&env);
    let payee = Address::generate(&env);

    for i in 1u64..=5 {
        client.create_payment(&i, &payer, &payee, &(i as i128 * 100));
    }

    let page0 = client.get_payments_by_payer(&payer, &0u32, &2u32);
    assert_eq!(page0.items.len(), 2);
    assert_eq!(page0.total, 5);

    let page1 = client.get_payments_by_payer(&payer, &1u32, &2u32);
    assert_eq!(page1.items.len(), 2);

    let page2 = client.get_payments_by_payer(&payer, &2u32, &2u32);
    assert_eq!(page2.items.len(), 1);
}

#[test]
fn test_get_payments_by_payer_crosses_fixed_index_page_boundary() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let payer = Address::generate(&env);
    let payee = Address::generate(&env);

    for request_id in 1u64..=120 {
        client.create_payment(&request_id, &payer, &payee, &100i128);
    }

    let first = client.get_payments_by_payer(&payer, &0u32, &100u32);
    assert_eq!(first.items.len(), 100);
    assert_eq!(first.total, 120);
    assert_eq!(first.items.get(0).unwrap().id, 1);
    assert_eq!(first.items.get(99).unwrap().id, 100);

    let second = client.get_payments_by_payer(&payer, &1u32, &100u32);
    assert_eq!(second.items.len(), 20);
    assert_eq!(second.items.get(0).unwrap().id, 101);
    assert_eq!(second.items.get(19).unwrap().id, 120);
}

#[test]
fn test_get_payments_by_payer_small_pages_span_fixed_index_pages() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let payer = Address::generate(&env);
    let payee = Address::generate(&env);

    for request_id in 1u64..=120 {
        client.create_payment(&request_id, &payer, &payee, &100i128);
    }

    let page = client.get_payments_by_payer(&payer, &5u32, &20u32);
    assert_eq!(page.items.len(), 20);
    assert_eq!(page.items.get(0).unwrap().id, 101);
    assert_eq!(page.items.get(19).unwrap().id, 120);
}

#[test]
fn test_payment_pagination_caps_requested_page_size() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let payer = Address::generate(&env);
    let payee = Address::generate(&env);

    for request_id in 1u64..=101 {
        client.create_payment(&request_id, &payer, &payee, &100i128);
    }

    let page = client.get_payments_by_payer(&payer, &0u32, &1000u32);
    assert_eq!(page.items.len(), 100);
    assert_eq!(page.page_size, 100);
}

// ── get_payments_by_payee ──────────────────────────────────────────────────────

#[test]
fn test_get_payments_by_payee_returns_only_payee_payments() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let payer = Address::generate(&env);
    let payee_a = Address::generate(&env);

    client.create_payment(&1u64, &payer, &payee_a, &100i128);
    client.create_payment(&2u64, &payer, &payee_a, &200i128);
    make_payment(&env, &client, 3, 300);

    let page = client.get_payments_by_payee(&payee_a, &0u32, &20u32);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.total, 2);
}

#[test]
fn test_get_payments_by_payee_pagination() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let payer = Address::generate(&env);
    let payee = Address::generate(&env);

    for i in 1u64..=6 {
        client.create_payment(&i, &payer, &payee, &(i as i128 * 50));
    }

    let page = client.get_payments_by_payee(&payee, &0u32, &4u32);
    assert_eq!(page.items.len(), 4);
    assert_eq!(page.total, 6);

    let page2 = client.get_payments_by_payee(&payee, &1u32, &4u32);
    assert_eq!(page2.items.len(), 2);
}

// ── get_payments_by_status ─────────────────────────────────────────────────────

#[test]
fn test_get_payments_by_status_filters_correctly() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let (id1, _, _) = make_payment(&env, &client, 1, 100);
    let (id2, _, _) = make_payment(&env, &client, 2, 200);
    make_payment(&env, &client, 3, 300);

    force_status(&env, &cid, id1, PaymentStatus::Locked);
    force_status(&env, &cid, id2, PaymentStatus::Locked);

    let locked = client.get_payments_by_status(&PaymentStatus::Locked, &0u32, &20u32);
    assert_eq!(locked.items.len(), 2);
    assert_eq!(locked.total, 2);

    let pending = client.get_payments_by_status(&PaymentStatus::Pending, &0u32, &20u32);
    assert_eq!(pending.items.len(), 1);
}

#[test]
fn test_get_payments_by_status_empty_when_none_match() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    make_payment(&env, &client, 1, 100);

    let page = client.get_payments_by_status(&PaymentStatus::Released, &0u32, &20u32);
    assert_eq!(page.items.len(), 0);
}

#[test]
fn test_get_payments_by_status_pagination() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    for i in 1u64..=5 {
        let (id, _, _) = make_payment(&env, &client, i, 100);
        force_status(&env, &cid, id, PaymentStatus::Refunded);
    }

    let page0 = client.get_payments_by_status(&PaymentStatus::Refunded, &0u32, &3u32);
    assert_eq!(page0.items.len(), 3);
    assert_eq!(page0.total, 5);

    let page1 = client.get_payments_by_status(&PaymentStatus::Refunded, &1u32, &3u32);
    assert_eq!(page1.items.len(), 2);
}

#[test]
fn test_status_index_removal_and_insertion_cross_page_boundary() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let mut ids: Vec<u64> = Vec::new(&env);

    for request_id in 1u64..=101 {
        let (id, _, _) = make_payment(&env, &client, request_id, 100);
        ids.push_back(id);
    }

    for i in 0..ids.len() {
        force_status(&env, &cid, ids.get(i).unwrap(), PaymentStatus::Refunded);
    }

    let refunded_first = client.get_payments_by_status(&PaymentStatus::Refunded, &0u32, &100u32);
    assert_eq!(refunded_first.items.len(), 100);
    assert_eq!(refunded_first.total, 101);

    let refunded_second = client.get_payments_by_status(&PaymentStatus::Refunded, &1u32, &100u32);
    assert_eq!(refunded_second.items.len(), 1);
    assert_eq!(refunded_second.total, 101);
}

// ── get_payment_statistics ─────────────────────────────────────────────────────

#[test]
fn test_statistics_empty_when_no_payments() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let stats = client.get_payment_statistics();
    assert_eq!(stats.total_locked, 0);
    assert_eq!(stats.total_released, 0);
    assert_eq!(stats.total_refunded, 0);
    assert_eq!(stats.count_locked, 0);
    assert_eq!(stats.count_released, 0);
    assert_eq!(stats.count_refunded, 0);
}

#[test]
fn test_statistics_counts_and_totals_correctly() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);

    let (id1, _, _) = make_payment(&env, &client, 1, 1000);
    let (id2, _, _) = make_payment(&env, &client, 2, 2000);
    let (id3, _, _) = make_payment(&env, &client, 3, 500);
    let (id4, _, _) = make_payment(&env, &client, 4, 750);
    make_payment(&env, &client, 5, 300); // stays Pending

    force_status(&env, &cid, id1, PaymentStatus::Locked);
    force_status(&env, &cid, id2, PaymentStatus::Locked);
    force_status(&env, &cid, id3, PaymentStatus::Released);
    force_status(&env, &cid, id4, PaymentStatus::Refunded);

    let stats = client.get_payment_statistics();
    assert_eq!(stats.count_locked, 2);
    assert_eq!(stats.total_locked, 3000);
    assert_eq!(stats.count_released, 1);
    assert_eq!(stats.total_released, 500);
    assert_eq!(stats.count_refunded, 1);
    assert_eq!(stats.total_refunded, 750);
}

#[test]
fn test_statistics_ignores_pending_cancelled_disputed() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let (id1, _, _) = make_payment(&env, &client, 1, 100);
    let (id2, _, _) = make_payment(&env, &client, 2, 200);
    make_payment(&env, &client, 3, 300); // stays Pending

    force_status(&env, &cid, id1, PaymentStatus::Cancelled);
    force_status(&env, &cid, id2, PaymentStatus::Disputed);

    let stats = client.get_payment_statistics();
    assert_eq!(stats.count_locked, 0);
    assert_eq!(stats.count_released, 0);
    assert_eq!(stats.count_refunded, 0);
    assert_eq!(stats.total_locked, 0);
}

// ── get_payment_timeline ───────────────────────────────────────────────────────

#[test]
fn test_timeline_returns_payments_in_chronological_order() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);

    env.ledger().with_mut(|l| l.timestamp = 3000);
    make_payment(&env, &client, 1, 100);

    env.ledger().with_mut(|l| l.timestamp = 1000);
    make_payment(&env, &client, 2, 200);

    env.ledger().with_mut(|l| l.timestamp = 2000);
    make_payment(&env, &client, 3, 300);

    let page = client.get_payment_timeline(&0u32, &20u32);
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.items.get(0).unwrap().created_at, 1000);
    assert_eq!(page.items.get(1).unwrap().created_at, 2000);
    assert_eq!(page.items.get(2).unwrap().created_at, 3000);
}

#[test]
fn test_timeline_pagination() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);

    for i in 1u64..=5 {
        env.ledger().with_mut(|l| l.timestamp = i * 1000);
        make_payment(&env, &client, i, 100);
    }

    let page0 = client.get_payment_timeline(&0u32, &2u32);
    assert_eq!(page0.items.len(), 2);
    assert_eq!(page0.total, 5);
    assert_eq!(page0.items.get(0).unwrap().created_at, 1000);

    let page1 = client.get_payment_timeline(&1u32, &2u32);
    assert_eq!(page1.items.len(), 2);
    assert_eq!(page1.items.get(0).unwrap().created_at, 3000);

    let page2 = client.get_payment_timeline(&2u32, &2u32);
    assert_eq!(page2.items.len(), 1);
    assert_eq!(page2.items.get(0).unwrap().created_at, 5000);
}

#[test]
fn test_timeline_empty_when_no_payments() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let page = client.get_payment_timeline(&0u32, &20u32);
    assert_eq!(page.items.len(), 0);
    assert_eq!(page.total, 0);
}

#[test]
fn test_timeline_out_of_range_page_returns_empty() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    make_payment(&env, &client, 1, 100);

    let page = client.get_payment_timeline(&99u32, &20u32);
    assert_eq!(page.items.len(), 0);
    assert_eq!(page.total, 1);
}

#[test]
fn test_timeline_reads_only_requested_page() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);

    for request_id in 1u64..=101 {
        make_payment(&env, &client, request_id, 100);
    }

    let page = client.get_payment_timeline(&1u32, &100u32);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items.get(0).unwrap().id, 101);
    assert_eq!(page.total, 101);
}

#[test]
fn test_batch_create_rejects_more_than_maximum_before_writes() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let mut payments: Vec<(u64, Address, Address, i128)> = Vec::new(&env);

    for request_id in 1u64..=101 {
        payments.push_back((
            request_id,
            Address::generate(&env),
            Address::generate(&env),
            100,
        ));
    }

    let result = client.try_batch_create_payments(&payments);
    assert_eq!(result, Err(Ok(Error::BatchTooLarge)));
    assert_eq!(client.get_payment_count(), 0);
}

// ── donation pledges ───────────────────────────────────────────────────────────

#[test]
fn test_create_pledge_stores_metadata() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let donor = Address::generate(&env);
    let pool = soroban_sdk::String::from_str(&env, "hospital-pool-42");
    let cause = soroban_sdk::String::from_str(&env, "maternal_health");
    let region = soroban_sdk::String::from_str(&env, "NG-Lagos");

    let id = client.create_pledge(
        &donor,
        &500i128,
        &2_592_000u64,
        &pool,
        &cause,
        &region,
        &true,
    );

    let p = client.get_pledge(&id);
    assert_eq!(p.donor, donor);
    assert_eq!(p.amount_per_period, 500);
    assert_eq!(p.interval_secs, 2_592_000);
    assert!(p.emergency_pool);
    assert!(p.active);
}

#[test]
fn test_create_pledge_rejects_zero_interval() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let donor = Address::generate(&env);
    let pool = soroban_sdk::String::from_str(&env, "pool");
    let cause = soroban_sdk::String::from_str(&env, "c");
    let region = soroban_sdk::String::from_str(&env, "r");
    let r = client.try_create_pledge(&donor, &100i128, &0u64, &pool, &cause, &region, &false);
    assert!(r.is_err());
}

// ── Circuit breaker tests ─────────────────────────────────────────────────────

#[test]
fn test_pause_blocks_create_payment() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let cid = env.register(PaymentContract, (&admin, &None::<Address>));
    let client = PaymentContractClient::new(&env, &cid);

    client.pause(&admin);
    assert!(client.is_paused());

    let payer = Address::generate(&env);
    let payee = Address::generate(&env);
    let result = client.try_create_payment(&1u64, &payer, &payee, &500i128);
    assert!(result.is_err());
}

#[test]
fn test_pause_allows_get_payment() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let cid = env.register(PaymentContract, (&admin, &None::<Address>));
    let client = PaymentContractClient::new(&env, &cid);

    let (id, _, _) = make_payment(&env, &client, 1, 1000);
    client.pause(&admin);

    // Read still works
    let p = client.get_payment(&id);
    assert_eq!(p.id, id);
}

#[test]
fn test_unpause_restores_payments() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let cid = env.register(PaymentContract, (&admin, &None::<Address>));
    let client = PaymentContractClient::new(&env, &cid);

    client.pause(&admin);
    client.unpause(&admin);
    assert!(!client.is_paused());

    let (id, _, _) = make_payment(&env, &client, 99, 200);
    assert!(id > 0);
}

#[test]
#[should_panic]
fn test_non_admin_cannot_pause_payments() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let cid = env.register(PaymentContract, (&admin, &None::<Address>));
    let client = PaymentContractClient::new(&env, &cid);

    let attacker = Address::generate(&env);
    client.pause(&attacker);
}

// ── Vesting schedule tests ─────────────────────────────────────────────────────

fn setup_with_admin() -> (Env, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    // Atomic deploy+init via __constructor.
    let contract_id = env.register(PaymentContract, (&admin, &None::<Address>));
    (env, contract_id, admin)
}

/// Pre-cliff claim must return CliffNotReached.
#[test]
fn test_vesting_pre_cliff_claim_fails() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    let donor = Address::generate(&env);

    // cliff = now + 1000s, duration = 2000s
    env.ledger().with_mut(|l| l.timestamp = 5000);
    client.create_vesting(&admin, &donor, &1_000_000i128, &1000u64, &2000u64);

    // Deploy reward token and mint to contract so it can transfer
    let token_id = deploy_token_with_balance(&env, &admin, &cid, 1_000_000);

    // Try to claim at t=5500 (before cliff at t=6000)
    env.ledger().with_mut(|l| l.timestamp = 5500);
    let result = client.try_claim_vested(&donor, &token_id);
    assert_eq!(
        result,
        Err(Ok(Error::CliffNotReached)),
        "Expected CliffNotReached before cliff"
    );
}

/// At 50% of vesting duration, claimable = total/2.
#[test]
fn test_vesting_partial_claim_at_50_percent() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    let donor = Address::generate(&env);

    // cliff = now + 0 (immediate), duration = 2000s → vest_end = now + 2000
    env.ledger().with_mut(|l| l.timestamp = 10_000);
    client.create_vesting(&admin, &donor, &1_000_000i128, &0u64, &2000u64);

    let token_id = deploy_token_with_balance(&env, &admin, &cid, 1_000_000);

    // Advance to 50% of vesting duration (cliff == vest_start == 10_000, vest_end == 12_000)
    env.ledger().with_mut(|l| l.timestamp = 11_000); // 1000s elapsed of 2000s
    let claimed = client.claim_vested(&donor, &token_id);
    assert_eq!(claimed, 500_000i128, "50% vesting should yield half the total");

    let schedule = client.get_vesting(&donor);
    assert_eq!(schedule.claimed, 500_000i128);
}

/// After vesting end, donor can claim the full remaining amount.
#[test]
fn test_vesting_full_claim_after_vest_end() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    let donor = Address::generate(&env);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    client.create_vesting(&admin, &donor, &500_000i128, &0u64, &1000u64);

    let token_id = deploy_token_with_balance(&env, &admin, &cid, 500_000);

    // Advance past vest_end
    env.ledger().with_mut(|l| l.timestamp = 3_000);
    let claimed = client.claim_vested(&donor, &token_id);
    assert_eq!(claimed, 500_000i128, "Full amount claimable after vest end");

    let schedule = client.get_vesting(&donor);
    assert_eq!(schedule.claimed, 500_000i128);
    assert_eq!(schedule.claimed, schedule.total_amount);
}

/// Donor cannot claim more than total_amount across multiple claims.
#[test]
fn test_vesting_cannot_exceed_total_amount() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    let donor = Address::generate(&env);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    client.create_vesting(&admin, &donor, &1_000_000i128, &0u64, &1000u64);

    let token_id = deploy_token_with_balance(&env, &admin, &cid, 1_000_000);

    // Claim full amount after vest end
    env.ledger().with_mut(|l| l.timestamp = 5_000);
    let first = client.claim_vested(&donor, &token_id);
    assert_eq!(first, 1_000_000i128);

    // Second claim should fail with NothingToClaim
    let result = client.try_claim_vested(&donor, &token_id);
    assert_eq!(
        result,
        Err(Ok(Error::NothingToClaim)),
        "Second claim after full vest should fail"
    );
}

/// Non-admin cannot create a vesting schedule.
#[test]
fn test_vesting_only_admin_can_create() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    let attacker = Address::generate(&env);
    let donor = Address::generate(&env);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let result = client.try_create_vesting(&attacker, &donor, &1_000i128, &100u64, &500u64);
    assert!(result.is_err(), "Non-admin must not create vesting");
}

// ── process_expired_disputes (#595) ─────────────────────────────────────────────────

#[test]
fn test_process_expired_disputes_refunds_after_timeout() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 10_000);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    // Record dispute at t=1000; updated_at becomes 1000.
    client.record_dispute(&hospital, &pid, &DisputeReason::FailedDelivery,
        &soroban_sdk::String::from_str(&env, "case-1"));

    // Set a short timeout of 500s.
    client.set_dispute_timeout(&admin, &500u64);

    // Advance time past timeout.
    env.ledger().with_mut(|l| l.timestamp = 2_000);

    let mut ids = soroban_sdk::Vec::new(&env);
    ids.push_back(pid);
    let refunded = client.process_expired_disputes(&admin, &ids);
    assert_eq!(refunded.len(), 1);
    assert_eq!(refunded.get(0).unwrap(), pid);

    let p = client.get_payment(&pid);
    assert_eq!(p.status, PaymentStatus::Refunded);
}

#[test]
fn test_process_expired_disputes_skips_non_expired() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 10_000);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let pid = client.create_escrow(&2u64, &hospital, &payee, &500i128, &token_id);
    client.record_dispute(&hospital, &pid, &DisputeReason::Other,
        &soroban_sdk::String::from_str(&env, "case-2"));

    client.set_dispute_timeout(&admin, &5_000u64);

    // Only 100s elapsed — not expired.
    env.ledger().with_mut(|l| l.timestamp = 1_100);

    let mut ids = soroban_sdk::Vec::new(&env);
    ids.push_back(pid);
    let refunded = client.process_expired_disputes(&admin, &ids);
    assert_eq!(refunded.len(), 0);

    let p = client.get_payment(&pid);
    assert_eq!(p.status, PaymentStatus::Disputed);
}

#[test]
fn test_process_expired_disputes_skips_non_disputed_payments() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let (pid, _, _) = make_payment(&env, &client, 3, 200);
    // Payment is Pending, not Disputed.
    client.set_dispute_timeout(&admin, &1u64);
    env.ledger().with_mut(|l| l.timestamp = 9_000);

    let mut ids = soroban_sdk::Vec::new(&env);
    ids.push_back(pid);
    let refunded = client.process_expired_disputes(&admin, &ids);
    assert_eq!(refunded.len(), 0);
}

// ── Coordinator role: authorization for escrow settlement ─────────────────────

#[test]
fn test_set_coordinator_allows_coordinator_to_release_and_refund() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    let coordinator = Address::generate(&env);
    client.set_coordinator(&admin, &coordinator);
    assert_eq!(client.get_coordinator(), Some(coordinator.clone()));

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 2_000);
    let pid1 = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);
    let pid2 = client.create_escrow(&2u64, &hospital, &payee, &1_000i128, &token_id);

    // Coordinator can release without being admin.
    client.release_escrow(&coordinator, &pid1);
    assert_eq!(client.get_payment(&pid1).status, PaymentStatus::Released);

    // Coordinator can refund without being admin.
    client.refund_escrow(&coordinator, &pid2);
    assert_eq!(client.get_payment(&pid2).status, PaymentStatus::Refunded);
}

#[test]
fn test_non_admin_cannot_set_coordinator() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    let attacker = Address::generate(&env);
    let coordinator = Address::generate(&env);
    let result = client.try_set_coordinator(&attacker, &coordinator);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_get_coordinator_defaults_to_none() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    assert_eq!(client.get_coordinator(), None);
}

// ── Escrow settlement: authorization & real token transfers ────────────────────

#[test]
fn test_release_escrow_transfers_tokens_to_payee() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    client.release_escrow(&admin, &pid);

    let token_client = soroban_sdk::token::Client::new(&env, &token_id);
    assert_eq!(token_client.balance(&payee), 1_000);
    assert_eq!(token_client.balance(&cid), 0);
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Released);
}

#[test]
fn test_refund_escrow_transfers_tokens_to_payer() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    client.refund_escrow(&admin, &pid);

    let token_client = soroban_sdk::token::Client::new(&env, &token_id);
    assert_eq!(token_client.balance(&hospital), 1_000);
    assert_eq!(token_client.balance(&cid), 0);
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Refunded);
}

#[test]
fn test_release_escrow_rejects_non_admin_non_coordinator() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    let attacker = Address::generate(&env);
    let result = client.try_release_escrow(&attacker, &pid);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_refund_escrow_rejects_non_admin_non_coordinator() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    let attacker = Address::generate(&env);
    let result = client.try_refund_escrow(&attacker, &pid);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

// ── Dispute lifecycle: authorization & Locked ⇄ Disputed transitions ──────────

#[test]
fn test_record_dispute_rejects_unauthorized_caller() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    let stranger = Address::generate(&env);
    let result = client.try_record_dispute(
        &stranger,
        &pid,
        &DisputeReason::Other,
        &soroban_sdk::String::from_str(&env, "case-x"),
    );
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_record_dispute_allows_payer() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    client.record_dispute(
        &hospital,
        &pid,
        &DisputeReason::FailedDelivery,
        &soroban_sdk::String::from_str(&env, "case-1"),
    );
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Disputed);
}

#[test]
fn test_record_dispute_allows_payee() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    client.record_dispute(
        &payee,
        &pid,
        &DisputeReason::WrongItem,
        &soroban_sdk::String::from_str(&env, "case-2"),
    );
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Disputed);
}

#[test]
fn test_record_dispute_allows_coordinator() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    let coordinator = Address::generate(&env);
    client.set_coordinator(&admin, &coordinator);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    client.record_dispute(
        &coordinator,
        &pid,
        &DisputeReason::TemperatureExcursion,
        &soroban_sdk::String::from_str(&env, "case-temp"),
    );
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Disputed);
}

#[test]
fn test_record_dispute_rejects_non_locked_payment() {
    let (env, cid) = setup();
    let client = PaymentContractClient::new(&env, &cid);
    let (pid, payer, _payee) = make_payment(&env, &client, 1, 500);
    // Payment is Pending — never escrowed, never Locked.
    let result = client.try_record_dispute(
        &payer,
        &pid,
        &DisputeReason::Other,
        &soroban_sdk::String::from_str(&env, "case-3"),
    );
    assert_eq!(result, Err(Ok(Error::PaymentNotLocked)));
}

#[test]
fn test_resolve_dispute_restores_locked_status() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);
    client.record_dispute(
        &hospital,
        &pid,
        &DisputeReason::DamagedGoods,
        &soroban_sdk::String::from_str(&env, "case-4"),
    );
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Disputed);

    client.resolve_dispute(&admin, &pid);

    let p = client.get_payment(&pid);
    assert_eq!(p.status, PaymentStatus::Locked);
    assert!(p.dispute_resolved);
}

#[test]
fn test_resolve_dispute_rejects_non_admin_non_coordinator() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);
    client.record_dispute(
        &hospital,
        &pid,
        &DisputeReason::LateDelivery,
        &soroban_sdk::String::from_str(&env, "case-5"),
    );

    let stranger = Address::generate(&env);
    let result = client.try_resolve_dispute(&stranger, &pid);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_resolve_dispute_rejects_non_disputed_payment() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);
    // Never disputed — still Locked.
    let result = client.try_resolve_dispute(&admin, &pid);
    assert_eq!(result, Err(Ok(Error::PaymentNotDisputed)));
}

/// A disputed escrow reaches Released through a real, authorized path after
/// resolution — it is never permanently stuck at Disputed.
#[test]
fn test_disputed_payment_can_be_released_after_resolution() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    client.record_dispute(
        &payee,
        &pid,
        &DisputeReason::PaymentContested,
        &soroban_sdk::String::from_str(&env, "case-6"),
    );
    client.resolve_dispute(&admin, &pid);
    client.release_escrow(&admin, &pid);

    let token_client = soroban_sdk::token::Client::new(&env, &token_id);
    assert_eq!(token_client.balance(&payee), 1_000);
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Released);
}

/// Same, but resolving into a refund instead of a release.
#[test]
fn test_disputed_payment_can_be_refunded_after_resolution() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    client.record_dispute(
        &hospital,
        &pid,
        &DisputeReason::TemperatureExcursion,
        &soroban_sdk::String::from_str(&env, "case-7"),
    );
    client.resolve_dispute(&admin, &pid);
    client.refund_escrow(&admin, &pid);

    let token_client = soroban_sdk::token::Client::new(&env, &token_id);
    assert_eq!(token_client.balance(&hospital), 1_000);
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Refunded);
}

/// VestingCreated and VestingClaimed events are emitted.
#[test]
fn test_vesting_events_emitted() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    let donor = Address::generate(&env);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    client.create_vesting(&admin, &donor, &200_000i128, &0u64, &1000u64);

    let token_id = deploy_token_with_balance(&env, &admin, &cid, 200_000);

    env.ledger().with_mut(|l| l.timestamp = 2_500); // past vest_end
    client.claim_vested(&donor, &token_id);

    // Events are published — verify no panic and schedule is updated
    let schedule = client.get_vesting(&donor);
    assert_eq!(schedule.claimed, 200_000i128);
}

// ── Storage TTL management (#52) ──────────────────────────────────────────────

/// Advance the ledger sequence by `ledgers` and the timestamp by `secs`.
fn fast_forward(env: &Env, ledgers: u32, secs: u64) {
    env.ledger().with_mut(|l| {
        l.sequence_number += ledgers;
        l.timestamp += secs;
    });
}

/// Advance the ledger sequence by `ledgers` only (timestamp unchanged).
fn fast_forward_ledgers(env: &Env, ledgers: u32) {
    env.ledger().with_mut(|l| l.sequence_number += ledgers);
}

/// A payment entry created at `lock` must stay live across the documented
/// dispute-resolution horizon. Advancing past the default TTL but staying
/// within the policy horizon must keep every entrypoint working.
#[test]
fn test_ttl_payment_live_past_default_ttl_within_horizon() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 10_000);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    // Jump well past the default persistent TTL but inside the policy horizon.
    fast_forward(&env, LEDGERS_PER_DAY * 10, 86_400 * 10);

    // Reads and settlement still succeed — no archival.
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Locked);
    client.release_escrow(&admin, &pid);
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Released);
}

/// Refund path must also survive the fast-forward within the horizon.
#[test]
fn test_ttl_refund_live_past_default_ttl_within_horizon() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    fast_forward(&env, LEDGERS_PER_DAY * 10, 86_400 * 10);

    client.refund_escrow(&admin, &pid);
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Refunded);
}

/// Boundary test: at horizon−1 the entry is still live and usable.
#[test]
fn test_ttl_payment_live_at_horizon_minus_one() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    // Advance to one ledger before the policy horizon.
    fast_forward_ledgers(&env, PAYMENT_POLICY_HORIZON_LEDGERS - 1);

    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Locked);
    client.release_escrow(&admin, &pid);
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Released);
}

/// Boundary test: at horizon+1 the entry is beyond the policy horizon and the
/// contract must surface the documented `StorageHorizonExceeded` error rather
/// than a host panic.
#[test]
fn test_ttl_payment_horizon_exceeded_at_horizon_plus_one() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);

    // Advance one ledger past the policy horizon.
    fast_forward_ledgers(&env, PAYMENT_POLICY_HORIZON_LEDGERS + 1);

    assert_eq!(
        client.try_get_payment(&pid),
        Err(Ok(Error::StorageHorizonExceeded))
    );
    assert_eq!(
        client.try_release_escrow(&admin, &pid),
        Err(Ok(Error::StorageHorizonExceeded))
    );
}

/// Lock-then-dispute flow spanning a simulated 30-day gap: the payment entry
/// must stay live through the dispute-resolution horizon so the dispute can be
/// recorded and resolved after the gap.
#[test]
fn test_ttl_lock_then_dispute_across_30_day_gap() {
    let (env, cid, admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hospital = Address::generate(&env);
    let payee = Address::generate(&env);
    let token_id = deploy_token_with_balance(&env, &admin, &hospital, 1_000);

    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let pid = client.create_escrow(&1u64, &hospital, &payee, &1_000i128, &token_id);
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Locked);

    // Simulate a 30-day gap — well past the default TTL, inside the horizon.
    fast_forward(&env, LEDGERS_PER_DAY * 30, 86_400 * 30);

    // The locked payment is still readable and disputable.
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Locked);
    client.record_dispute(
        &hospital,
        &pid,
        &DisputeReason::FailedDelivery,
        &soroban_sdk::String::from_str(&env, "case-ttl"),
    );
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Disputed);

    client.resolve_dispute(&admin, &pid);
    client.release_escrow(&admin, &pid);
    assert_eq!(client.get_payment(&pid).status, PaymentStatus::Released);
}

// ── Timelocked upgradeability & versioned schema (#31) ─────────────────────────

#[test]
fn test_version_and_default_schema_version() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    assert_eq!(client.version(), CONTRACT_VERSION);
    assert_eq!(client.schema_version(), 1);
}

#[test]
fn test_propose_upgrade_queues_behind_timelock() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    env.ledger().with_mut(|l| l.timestamp = 10_000);

    let hash = soroban_sdk::BytesN::from_array(&env, &[7u8; 32]);
    let executable_at = client.propose_upgrade(&hash);
    assert_eq!(executable_at, 10_000 + UPGRADE_TIMELOCK_SECS);

    let pending = client.get_pending_upgrade().unwrap();
    assert_eq!(pending.new_wasm_hash, hash);
    assert_eq!(pending.proposed_at, 10_000);
    assert_eq!(pending.executable_at, executable_at);
}

#[test]
fn test_execute_upgrade_before_timelock_elapses_fails() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    env.ledger().with_mut(|l| l.timestamp = 10_000);

    let hash = soroban_sdk::BytesN::from_array(&env, &[7u8; 32]);
    let executable_at = client.propose_upgrade(&hash);

    env.ledger().with_mut(|l| l.timestamp = executable_at - 1);
    assert_eq!(
        client.try_execute_upgrade(),
        Err(Ok(Error::TimelockNotElapsed))
    );
    // Still queued — nothing was consumed by the failed attempt.
    assert!(client.get_pending_upgrade().is_some());
}

#[test]
fn test_propose_upgrade_twice_fails() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hash = soroban_sdk::BytesN::from_array(&env, &[7u8; 32]);
    client.propose_upgrade(&hash);
    assert_eq!(
        client.try_propose_upgrade(&hash),
        Err(Ok(Error::UpgradeAlreadyPending))
    );
}

#[test]
fn test_cancel_upgrade_clears_pending_proposal() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    let hash = soroban_sdk::BytesN::from_array(&env, &[7u8; 32]);
    client.propose_upgrade(&hash);
    client.cancel_upgrade();

    assert!(client.get_pending_upgrade().is_none());
    assert_eq!(
        client.try_execute_upgrade(),
        Err(Ok(Error::NoPendingUpgrade))
    );
}

#[test]
fn test_execute_upgrade_without_proposal_fails() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    assert_eq!(
        client.try_execute_upgrade(),
        Err(Ok(Error::NoPendingUpgrade))
    );
}

#[test]
fn test_migrate_refuses_to_run_twice() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);

    // Simulate storage written by an older binary (schema 0 < target).
    env.as_contract(&cid, || {
        env.storage()
            .instance()
            .set(&crate::SCHEMA_VERSION_KEY, &0u32)
    });
    assert_eq!(client.schema_version(), 0);

    assert_eq!(client.migrate(), TARGET_SCHEMA_VERSION);
    assert_eq!(client.schema_version(), TARGET_SCHEMA_VERSION);

    // Double-run guard: a second invocation is refused.
    assert_eq!(
        client.try_migrate(),
        Err(Ok(Error::MigrationAlreadyApplied))
    );
}

#[test]
fn test_migrate_refused_at_current_schema() {
    let (env, cid, _admin) = setup_with_admin();
    let client = PaymentContractClient::new(&env, &cid);
    assert_eq!(
        client.try_migrate(),
        Err(Ok(Error::MigrationAlreadyApplied))
    );
}

/// Full upgrade rehearsal against the real compiled WASM: populate an open
/// escrow, propose → timelock → execute the upgrade, then prove the
/// in-flight escrow still settles correctly on the new binary.
///
/// Requires the workspace WASMs to be built first — run via
/// `scripts/test-upgrade-rehearsal.sh`.
#[cfg(feature = "upgrade-rehearsal")]
mod upgrade_rehearsal {
    use super::*;

    const PAYMENT_WASM: &[u8] = include_bytes!(
        "../../../target/wasm32v1-none/release/payment_contract.wasm"
    );

    #[test]
    fn test_upgrade_rehearsal_inflight_escrow_settles_after_upgrade() {
        let (env, cid, admin) = setup_with_admin();
        let client = PaymentContractClient::new(&env, &cid);
        env.ledger().with_mut(|l| l.timestamp = 1_000_000);

        // Realistic in-flight state: an open escrow holding donor funds.
        let hospital = Address::generate(&env);
        let payee = Address::generate(&env);
        let token = deploy_token_with_balance(&env, &admin, &hospital, 5_000);
        let payment_id = client.create_escrow(&1u64, &hospital, &payee, &5_000i128, &token);

        // Propose → wait out the 48h timelock → execute the WASM swap.
        let wasm_hash = env.deployer().upload_contract_wasm(PAYMENT_WASM);
        let executable_at = client.propose_upgrade(&wasm_hash);
        env.ledger().with_mut(|l| l.timestamp = executable_at + 1);
        client.execute_upgrade();

        // Same contract ID, storage intact, new binary answering.
        assert_eq!(client.version(), CONTRACT_VERSION);
        let payment = client.get_payment(&payment_id);
        assert_eq!(payment.status, PaymentStatus::Locked);
        assert_eq!(payment.amount, 5_000);

        // Schema unchanged between identical binaries — migrate must refuse.
        assert_eq!(
            client.try_migrate(),
            Err(Ok(Error::MigrationAlreadyApplied))
        );

        // The in-flight escrow completes correctly post-upgrade.
        client.release_escrow(&admin, &payment_id);
        let token_client = soroban_sdk::token::Client::new(&env, &token);
        assert_eq!(token_client.balance(&payee), 5_000i128);
        assert_eq!(client.get_payment(&payment_id).status, PaymentStatus::Released);
    }
}
