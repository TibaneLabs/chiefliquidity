//! Integration tests for the manipulation-resistant reference price
//! (`Pool::ref_price_wad`, DESIGN.md §12) and the swap-correctness fixes that
//! ship with it: post-swap trigger pricing that matches the settled reserves,
//! and rejection of zero-output swaps.

mod common;

use chiefliquidity::{
    error::LiquidityError,
    math::{
        cpmm_quote_out, ema_ref_price_wad, price_b_per_a_wad, recompute_trigger, LoanSides,
        BPS_DENOM, REF_PRICE_WINDOW_SLOTS,
    },
    state::Loan,
};
use common::{err_code, extract_custom_error, TestEnv};
use solana_sdk::signature::Signer;

const COLL_A: u8 = 0;

/// Accounted spot price (B per A, WAD) as the program would compute it now.
async fn spot_price_wad(env: &mut TestEnv) -> u128 {
    let pool = env.pool_state().await;
    let real_a = env.token_balance(&env.vault_a_pda().0).await as u128;
    let real_b = env.token_balance(&env.vault_b_pda().0).await as u128;
    let (acc_a, acc_b) = pool.accounted(real_a, real_b).unwrap();
    price_b_per_a_wad(acc_a, acc_b).unwrap()
}

// ============ Reference price maintenance ============

#[tokio::test]
async fn ref_price_seeded_by_first_swap_then_slot_sampled_ema() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    // Fresh pool: unset.
    assert_eq!(env.pool_state().await.ref_price_wad, 0);

    let (trader, ta, tb, _) = env
        .setup_user(10_000_000_000, 1_000_000_000, 4_000_000_000)
        .await;

    // First swap seeds the reference with the PRE-swap spot price.
    let p0 = spot_price_wad(&mut env).await;
    let slot0 = env.current_slot().await;
    env.swap_full(&trader, &ta, &tb, 100_000_000, 1, true)
        .await
        .unwrap();
    let pool = env.pool_state().await;
    assert_eq!(pool.ref_price_wad, p0);
    assert_eq!(pool.ref_price_slot, slot0);
    assert!(spot_price_wad(&mut env).await < p0, "swap moved spot");

    // A second swap in the same slot does not sample.
    env.swap_full(&trader, &ta, &tb, 100_000_000, 1, true)
        .await
        .unwrap();
    assert_eq!(env.current_slot().await, slot0);
    assert_eq!(env.pool_state().await.ref_price_wad, p0);

    // One slot later the first swap moves the reference by 1/H of the gap
    // to the pre-swap spot.
    env.warp_slots(1).await;
    let p1 = spot_price_wad(&mut env).await;
    env.swap_full(&trader, &ta, &tb, 10_000_000, 1, false)
        .await
        .unwrap();
    let pool = env.pool_state().await;
    let expected = ema_ref_price_wad(p0, p1, 1).unwrap();
    assert_eq!(pool.ref_price_wad, expected);
    assert_eq!(pool.ref_price_slot, slot0 + 1);
    assert!(expected < p0 && expected > p1);
    assert!(p0 - expected <= (p0 - p1) / REF_PRICE_WINDOW_SLOTS as u128 + 1);

    // After a full window of no swaps, the next swap snaps it to spot.
    env.warp_slots(REF_PRICE_WINDOW_SLOTS).await;
    let p2 = spot_price_wad(&mut env).await;
    env.swap_full(&trader, &ta, &tb, 10_000_000, 1, false)
        .await
        .unwrap();
    assert_eq!(env.pool_state().await.ref_price_wad, p2);
}

// ============ B1: pump-then-borrow ============

/// The attack from the audit: pump the pool price with a (flash-loanable)
/// swap and open a loan against the freshly bought collateral in the same
/// slot. Spot says the collateral is worth ~4×; the reference still says 1×,
/// so the loan is rejected. An honest borrow sized against the reference
/// still works in the same pumped state.
#[tokio::test]
async fn pump_then_borrow_same_slot_rejected_honest_borrow_ok() {
    let mut env = TestEnv::new().await;
    // 1:1 pool.
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 1_000_000_000)
        .await;
    // The reference is still unset (as on every pool right after the
    // upgrade): the pump swap itself seeds it with the pre-pump price.
    assert_eq!(env.pool_state().await.ref_price_wad, 0);
    let honest = spot_price_wad(&mut env).await;

    // Attacker pumps: 1e9 B in → ~5e8 A out, price B/A ≈ 4×.
    let (attacker, aa, ab, _) = env.setup_user(10_000_000_000, 0, 1_000_000_000).await;
    let slot = env.current_slot().await;
    env.swap_full(&attacker, &aa, &ab, 1_000_000_000, 1, false)
        .await
        .unwrap();
    let pumped = spot_price_wad(&mut env).await;
    assert!(pumped > 39 * honest / 10, "pumped={pumped} honest={honest}");
    assert_eq!(env.pool_state().await.ref_price_wad, honest);

    // Borrow 0.79 × collateral × spot: fine at spot (79%), ~316% at ref.
    let collateral = env.token_balance(&aa).await;
    let debt = (collateral as u128 * pumped / chiefliquidity::math::WAD * 79 / 100) as u64;
    assert!(
        debt > collateral,
        "attack would extract more B than A posted"
    );
    let err = env
        .open_loan(&attacker, &aa, &ab, COLL_A, collateral, debt)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::LtvExceedsMax))
    );
    assert_eq!(env.current_slot().await, slot, "attack ran in one slot");

    // Honest borrow in the same pumped state: 50% of collateral valued at the
    // reference (and far less at spot) → accepted.
    let honest_debt = collateral / 2;
    env.open_loan(&attacker, &aa, &ab, COLL_A, collateral, honest_debt)
        .await
        .unwrap();
    assert_eq!(env.pool_state().await.open_loans, 1);
}

/// Without any swap in the current slot, `OpenLoan` evaluates the reference
/// a dust swap would have produced — so a price sustained for a full window
/// is fully usable, while a one-slot-old pump moves it by only 1/H.
#[tokio::test]
async fn sustained_price_becomes_borrowable() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 1_000_000_000)
        .await;
    let (user, ua, ub, _) = env.setup_user(10_000_000_000, 0, 1_000_000_000).await;
    // Move the price 4× (seeds ref at 1×).
    env.swap_full(&user, &ua, &ub, 1_000_000_000, 1, false)
        .await
        .unwrap();
    let spot = spot_price_wad(&mut env).await;
    let collateral = env.token_balance(&ua).await / 2;
    // 75% LTV at spot ≈ 300% at the 1× reference.
    let debt = (collateral as u128 * spot / chiefliquidity::math::WAD * 75 / 100) as u64;

    // One slot later: the projected reference has barely moved.
    env.warp_slots(1).await;
    let err = env
        .open_loan(&user, &ua, &ub, COLL_A, collateral, debt)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::LtvExceedsMax))
    );

    // After a full window at the new price, the same loan is accepted.
    env.warp_slots(REF_PRICE_WINDOW_SLOTS).await;
    env.open_loan(&user, &ua, &ub, COLL_A, collateral, debt)
        .await
        .unwrap();
}

// ============ B2: trigger priced at the settled reserves ============

/// A loan whose trigger sits between the post-swap price the old code used
/// (input reserve + fee-reduced input) and the price the pool actually
/// settles at (input reserve + input − protocol skim) must be liquidated.
#[tokio::test]
async fn trigger_in_lp_fee_gap_is_liquidated() {
    let mut env = TestEnv::new().await;
    let r: u128 = 1_000_000_000;
    let _ = env.setup_pool_with_liquidity(r as u64, r as u64).await;

    // Quote a 1e8 A → B swap against the (loan-invariant) accounted reserves.
    let amount_in: u128 = 100_000_000;
    let out = cpmm_quote_out(amount_in, r, r, 30).unwrap();
    let after_fee = amount_in * (BPS_DENOM - 30) / BPS_DENOM;
    let protocol = amount_in * 30 / BPS_DENOM * 5 / 30;
    let p_old = price_b_per_a_wad(r + after_fee, r - out).unwrap();
    let p_true = price_b_per_a_wad(r + amount_in - protocol, r - out).unwrap();
    assert!(p_true < p_old);

    // CollateralA loan with trigger = debt * 1.1 / collateral ≈ midpoint.
    let collateral: u128 = 100_000_000;
    let target = (p_old + p_true) / 2;
    let debt = target * collateral * 10_000 / (11_000 * chiefliquidity::math::WAD);
    let (trigger, _) = recompute_trigger(LoanSides::CollateralA, collateral, debt, 11_000).unwrap();
    assert!(
        p_true <= trigger && trigger < p_old,
        "trigger {trigger} must sit in the gap [{p_true}, {p_old})"
    );

    let (borrower, ba, bb, _) = env.setup_user(10_000_000_000, collateral as u64, 0).await;
    let nonce = env
        .open_loan(&borrower, &ba, &bb, COLL_A, collateral as u64, debt as u64)
        .await
        .unwrap();
    let (loan_pda, _) = env.loan_pda(&borrower.pubkey(), nonce);

    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, amount_in as u64, 0).await;
    env.swap_full(&trader, &ta, &tb, amount_in as u64, 1, true)
        .await
        .unwrap();

    let loan = env.loan_state(&loan_pda).await.unwrap();
    assert_eq!(loan.status, Loan::STATUS_LIQUIDATED);
    assert_eq!(env.pool_state().await.open_loans, 0);
}

// ============ B3: zero-output swap ============

#[tokio::test]
async fn zero_output_swap_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 10, 10).await;

    // 1 B in → 0.997 B after fee → floors to 0 A out. With min_out = 0 this
    // used to take the user's token for nothing.
    let err = env
        .swap_full(&trader, &ta, &tb, 1, 0, false)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::ZeroAmount))
    );
    assert_eq!(env.token_balance(&tb).await, 10);
}
