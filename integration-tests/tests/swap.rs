//! Integration tests for `Swap`, including the mandatory in-flight
//! liquidation logic and band-completeness checks.

mod common;

use chiefliquidity::{error::LiquidityError, state::Loan};
use common::{err_code, extract_custom_error, TestEnv};
use solana_sdk::signature::Signer;

const COLL_A: u8 = 0;
const COLL_B: u8 = 1;

// ============ Simple swap (no loans, no liquidation) ============

#[tokio::test]
async fn swap_a_to_b_happy_no_loans() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 10_000_000, 0).await;
    let pre_a = env.token_balance(&ta).await;
    let pre_b = env.token_balance(&tb).await;

    env.swap_full(&trader, &ta, &tb, 10_000_000, 1, true)
        .await
        .unwrap();

    let post_a = env.token_balance(&ta).await;
    let post_b = env.token_balance(&tb).await;
    assert_eq!(pre_a - post_a, 10_000_000); // gave 10M A
    assert!(post_b - pre_b > 0); // received some B
    // Quote: 10M * 4e9 * 9970/10000 / (1e9 + 10M*9970/10000) ≈ 39.5M B
    let received = post_b - pre_b;
    assert!(
        received > 39_000_000 && received < 40_000_000,
        "received={received}"
    );
}

#[tokio::test]
async fn swap_b_to_a_happy_no_loans() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 0, 40_000_000).await;
    env.swap_full(&trader, &ta, &tb, 40_000_000, 1, false)
        .await
        .unwrap();

    let received_a = env.token_balance(&ta).await;
    // Quote: 40M * 1e9 * 9970/10000 / (4e9 + 40M*9970/10000) ≈ 9.87M A
    assert!(
        received_a > 9_800_000 && received_a < 10_000_000,
        "received={received_a}"
    );
}

#[tokio::test]
async fn swap_protocol_fee_accumulates() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    let pre_pool = env.pool_state().await;
    assert_eq!(pre_pool.protocol_fees_a, 0);

    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 100_000_000, 0).await;
    env.swap_full(&trader, &ta, &tb, 100_000_000, 1, true)
        .await
        .unwrap();

    let post_pool = env.pool_state().await;
    // swap_fee_bps=30, protocol_fee_bps=5 → 1/6 of the fee.
    // fee_taken = 100M * 30 / 10000 = 300_000
    // protocol_portion = 300_000 * 5 / 30 = 50_000
    assert_eq!(post_pool.protocol_fees_a, 50_000);
    assert_eq!(post_pool.protocol_fees_b, 0);
}

#[tokio::test]
async fn swap_slippage_breach_reverts() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 100_000_000, 0).await;
    // Demand min_out way above what's possible.
    let err = env
        .swap_full(&trader, &ta, &tb, 100_000_000, 1_000_000_000, true)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::SlippageExceeded))
    );
}

#[tokio::test]
async fn swap_zero_amount_in_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 100, 0).await;
    let err = env
        .swap_full(&trader, &ta, &tb, 0, 1, true)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::ZeroAmount))
    );
}

// ============ Swap that triggers liquidation ============

#[tokio::test]
async fn swap_a_to_b_triggers_collateral_a_liquidation() {
    let mut env = TestEnv::new().await;
    // Big pool so a single open_loan doesn't shift price too much.
    // 1B A and 4B B → price = 4 B/A.
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    // Borrower opens an A-collateralized loan that would liquidate when price
    // falls. With 110% liq ratio, trigger = (debt * 1.1) / collateral.
    // collateral 100M A, debt 350M B → trigger = 350M * 1.1 / 100M = 3.85 B/A
    // But max_ltv check would fail (350/(100*4) = 87.5% > 80%).
    // Use collateral 100M A, debt 300M B → ltv = 300/(100*4) = 75% ≤ 80% ✓
    // → trigger = 300*1.1/100 = 3.3 B/A. Liquidates when price ≤ 3.3.
    let (borrower, b_a, b_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 0).await;
    let nonce = env
        .open_loan(&borrower, &b_a, &b_b, COLL_A, 100_000_000, 300_000_000)
        .await
        .unwrap();

    // Pre-state: open loan
    let pool_pre = env.pool_state().await;
    assert_eq!(pool_pre.open_loans, 1);
    assert_eq!(pool_pre.total_debt_b, 300_000_000);
    assert_eq!(pool_pre.total_collateral_a, 100_000_000);

    // After open: vault_a=1B+100M=1.1B, vault_b=4B-300M=3.7B.
    // accounted_a = (1.1B - 100M_coll) + 0_debt = 1B
    // accounted_b = (3.7B - 0_coll) + 300M_debt = 4B
    // price = 4 B/A (unchanged — borrowing doesn't move price). ✓

    // Now an a_to_b swap that pushes price down past 3.3.
    // Need accounted_b/accounted_a < 3.3. With current 4B/1B = 4, need a swap
    // that makes new ratio < 3.3.
    // Sell 100M A. Pre-fee in_after_fee=100M*9970/10000=99.7M.
    // amount_out = 99.7M * 4B / (1B + 99.7M) ≈ 362.6M
    // post: a=1.0997B, b=3.637B → price = 3.31. Just barely above 3.3 →
    // wouldn't trigger. Try 200M:
    // in_after_fee=199.4M, out=199.4M*4B/(1B+199.4M)=664.4M
    // post: a=1.1994B, b=3.336B → price = 2.78. Triggers!
    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 300_000_000, 0).await;
    env.swap_full(&trader, &ta, &tb, 200_000_000, 1, true)
        .await
        .unwrap();

    // Post-state: loan is liquidated, totals zero out
    let pool_post = env.pool_state().await;
    assert_eq!(pool_post.open_loans, 0);
    assert_eq!(pool_post.total_debt_b, 0);
    assert_eq!(pool_post.total_collateral_a, 0);

    // Loan tombstone with STATUS_LIQUIDATED
    let (loan_pda, _) = env.loan_pda(&borrower.pubkey(), nonce);
    let loan = env.loan_state(&loan_pda).await.unwrap();
    assert_eq!(loan.status, Loan::STATUS_LIQUIDATED);
    assert_eq!(loan.collateral_amount, 0); // zeroed in tombstone
    assert_eq!(loan.debt_principal, 0);
    assert_eq!(loan.borrower, borrower.pubkey()); // borrower preserved
}

#[tokio::test]
async fn liquidated_borrower_can_claim_rent() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    let (borrower, b_a, b_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 0).await;
    let nonce = env
        .open_loan(&borrower, &b_a, &b_b, COLL_A, 100_000_000, 300_000_000)
        .await
        .unwrap();

    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 300_000_000, 0).await;
    env.swap_full(&trader, &ta, &tb, 200_000_000, 1, true)
        .await
        .unwrap();

    // Pre-claim: the tombstoned loan still holds its rent lamports.
    let (loan_pda, _) = env.loan_pda(&borrower.pubkey(), nonce);
    let pre_loan_lamports = env
        .banks_client
        .get_account(loan_pda)
        .await
        .unwrap()
        .unwrap()
        .lamports;
    assert!(pre_loan_lamports > 0);

    let pre_borrower_lamports = env
        .banks_client
        .get_account(borrower.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;

    env.claim_liquidated_rent(&borrower, nonce).await.unwrap();

    // Borrower's lamport balance grew by ~ pre_loan + pre_link (minus tx fee).
    let post_borrower_lamports = env
        .banks_client
        .get_account(borrower.pubkey())
        .await
        .unwrap()
        .unwrap()
        .lamports;
    let recovered = post_borrower_lamports - pre_borrower_lamports;
    let total = pre_loan_lamports;
    // Expect >90% recovered (subtract small tx fee).
    assert!(
        recovered as f64 > total as f64 * 0.9,
        "recovered={recovered} total={total}"
    );
}

// ============ Adversarial: skip a populated band ============

#[tokio::test]
async fn swap_skipping_populated_band_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    // Open a CollateralA loan so band_bitmap_fall has a populated band.
    let (borrower, b_a, b_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 0).await;
    env.open_loan(&borrower, &b_a, &b_b, COLL_A, 100_000_000, 300_000_000)
        .await
        .unwrap();

    // Now a_to_b swap. The completeness check requires the populated
    // OnFall band to be supplied. We pass `bands = []` and boundary = 0,
    // which means the program scans bitmap[0..=MAX] and finds the
    // populated band → IncompleteBandWalk.
    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 200_000_000, 0).await;
    let err = env
        .swap(&trader, &ta, &tb, 200_000_000, 1, true, 0, &[])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::IncompleteBandWalk))
    );
}

// ============ Regression: reopening a swap-emptied band ============
//
// A swap that liquidates a band's last loan clears the band's bitmap bit but
// leaves the band PDA allocated (count = 0, rent recoverable later). A loan
// subsequently opened into that band must re-set the bit — otherwise the loan
// is invisible to the completeness proof and can never be liquidated.

#[tokio::test]
async fn reopened_band_after_liquidation_is_tracked_in_bitmap() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    // Loan 1: coll 100M A, debt 300M B → trigger 3.3 B/A (band 66, OnFall).
    let (borrower1, b1_a, b1_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 0).await;
    env.open_loan(&borrower1, &b1_a, &b1_b, COLL_A, 100_000_000, 300_000_000)
        .await
        .unwrap();
    let (trigger1, _) = chiefliquidity::math::recompute_trigger(
        chiefliquidity::math::LoanSides::CollateralA,
        100_000_000,
        300_000_000,
        11_000,
    )
    .unwrap();
    let band_id = chiefliquidity::math::band_id_for_trigger(trigger1).unwrap();

    // Swap A→B liquidates loan 1, emptying the band: bit cleared, PDA kept.
    let (trader, t_a, t_b, _) = env.setup_user(10_000_000_000, 300_000_000, 0).await;
    env.swap_full(&trader, &t_a, &t_b, 200_000_000, 1, true)
        .await
        .unwrap();

    let pool = env.pool_state().await;
    assert_eq!(pool.open_loans, 0);
    assert!(
        !chiefliquidity::state::bitmap_is_set(&pool.band_bitmap_fall, band_id),
        "emptied band's bit must be cleared"
    );
    let band = env.band_state(0, band_id).await.expect("band PDA persists");
    assert_eq!(band.count, 0);

    // Push the price back up (B→A) so a new loan's trigger can land in the
    // same band. Post-liquidation price ≈ 2.41; this brings it to ≈ 3.6.
    let (pumper, p_a, p_b, _) = env.setup_user(10_000_000_000, 0, 700_000_000).await;
    env.swap_full(&pumper, &p_a, &p_b, 700_000_000, 1, false)
        .await
        .unwrap();

    // Loan 2: coll 100M A, debt 250M B → trigger 2.75 — same band 66.
    let (borrower2, b2_a, b2_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 0).await;
    env.open_loan(&borrower2, &b2_a, &b2_b, COLL_A, 100_000_000, 250_000_000)
        .await
        .unwrap();
    let (trigger2, _) = chiefliquidity::math::recompute_trigger(
        chiefliquidity::math::LoanSides::CollateralA,
        100_000_000,
        250_000_000,
        11_000,
    )
    .unwrap();
    assert_eq!(
        chiefliquidity::math::band_id_for_trigger(trigger2).unwrap(),
        band_id,
        "test setup must reuse the emptied band"
    );

    // The reused band must be visible again in the pool bitmap…
    let pool = env.pool_state().await;
    assert!(
        chiefliquidity::state::bitmap_is_set(&pool.band_bitmap_fall, band_id),
        "reopened band's bit must be re-set"
    );
    let band = env.band_state(0, band_id).await.unwrap();
    assert_eq!(band.count, 1);

    // …so a swap omitting it is rejected rather than silently skipping the loan.
    let (trader2, t2_a, t2_b, _) = env.setup_user(10_000_000_000, 200_000_000, 0).await;
    let err = env
        .swap(&trader2, &t2_a, &t2_b, 200_000_000, 1, true, 0, &[])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::IncompleteBandWalk))
    );
}

// ============ Adversarial: too many liquidations ============
//
// Hitting MAX_LIQ_PER_SWAP = 8 requires opening 9+ loans that all trigger on
// the same swap. Each loan needs a healthy LTV at open. With our 80% max LTV
// and 110% liq ratio, the trigger price is at LTV = 1/1.1 ≈ 90.9%. The gap
// between 80% and ~91% is small, so a moderate price move triggers all of
// them. Skipped here for setup brevity — would need ~10 borrower setups.

// ============ Adversarial: directional mismatch ============

#[tokio::test]
async fn swap_b_to_a_does_not_liquidate_collateral_a_loan() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    // Open a CollateralA / OnFall loan.
    let (borrower, b_a, b_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 0).await;
    env.open_loan(&borrower, &b_a, &b_b, COLL_A, 100_000_000, 300_000_000)
        .await
        .unwrap();

    // b_to_a swap raises price. CollateralA loans (OnFall) should NOT be
    // touched. Pool's band_bitmap_rise is empty, so completeness check
    // passes vacuously even with bands=[].
    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 0, 100_000_000).await;
    env.swap(&trader, &ta, &tb, 100_000_000, 1, false, 127, &[])
        .await
        .unwrap();

    // Loan is still alive
    let pool_post = env.pool_state().await;
    assert_eq!(pool_post.open_loans, 1);
    assert_eq!(pool_post.total_debt_b, 300_000_000);
    assert_eq!(pool_post.total_collateral_a, 100_000_000);
}
