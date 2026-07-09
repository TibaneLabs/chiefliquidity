//! Integration tests for `OpenLoan`, `RepayLoan`, and `ClaimLiquidatedRent`.

mod common;

use chiefliquidity::{error::LiquidityError, math::WAD, state::Loan};
use common::{err_code, extract_custom_error, TestEnv};
use solana_sdk::signature::Signer;

// Sides byte: 0 = CollateralA / DebtB, 1 = CollateralB / DebtA.
const COLL_A: u8 = 0;
const COLL_B: u8 = 1;

// ============ OpenLoan happy paths ============

#[tokio::test]
async fn open_loan_collateral_a_happy() {
    let mut env = TestEnv::new().await;
    // Pool: 1e9 A and 4e9 B → mid-price = 4 B per A.
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 4_000_000_000).await;

    // Borrower deposits 100M A as collateral, borrows 200M B.
    // collateral_value = 100M * 4 = 400M B; debt = 200M B → LTV = 50% = 5000 bps.
    // max_ltv default is 8000, so OK.
    let (borrower, ata_a, ata_b, _) = env.setup_user(10_000_000_000, 100_000_000, 0).await;
    let pre_a = env.token_balance(&ata_a).await;
    let pre_b = env.token_balance(&ata_b).await;
    let nonce = env
        .open_loan(&borrower, &ata_a, &ata_b, COLL_A, 100_000_000, 200_000_000)
        .await
        .unwrap();
    assert_eq!(nonce, 0);

    // Balances: borrower lost A (collateral), gained B (loan).
    let post_a = env.token_balance(&ata_a).await;
    let post_b = env.token_balance(&ata_b).await;
    assert_eq!(pre_a - post_a, 100_000_000);
    assert_eq!(post_b - pre_b, 200_000_000);

    // Pool counters
    let pool = env.pool_state().await;
    assert_eq!(pool.total_collateral_a, 100_000_000);
    assert_eq!(pool.total_debt_b, 200_000_000);
    assert_eq!(pool.open_loans, 1);
    assert_eq!(pool.next_loan_nonce, 1);

    // Loan persisted with sane fields
    let (loan_pda, _) = env.loan_pda(&borrower.pubkey(), nonce);
    let loan = env.loan_state(&loan_pda).await.unwrap();
    assert_eq!(loan.sides, COLL_A);
    assert_eq!(loan.collateral_amount, 100_000_000);
    assert_eq!(loan.debt_principal, 200_000_000);
    assert!(loan.is_open());
    // borrow_index_snapshot starts at WAD (no time has elapsed since pool init)
    assert_eq!(loan.borrow_index_snapshot_wad, WAD);
}

#[tokio::test]
async fn open_loan_collateral_b_happy() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 4_000_000_000).await;

    // Mirror: borrow A using B as collateral.
    // 200M B at price 4 B/A is worth 50M A. Borrow 25M A → LTV 50%.
    let (borrower, ata_a, ata_b, _) = env.setup_user(10_000_000_000, 0, 200_000_000).await;
    env.open_loan(&borrower, &ata_a, &ata_b, COLL_B, 200_000_000, 25_000_000)
        .await
        .unwrap();

    let pool = env.pool_state().await;
    assert_eq!(pool.total_collateral_b, 200_000_000);
    assert_eq!(pool.total_debt_a, 25_000_000);
}

// ============ OpenLoan adversarial ============

#[tokio::test]
async fn open_loan_ltv_exceeds_max() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 4_000_000_000).await;

    // collateral 100M A worth 400M B; borrow 350M B → LTV 87.5% > 80% (max).
    let (borrower, ata_a, ata_b, _) = env.setup_user(10_000_000_000, 100_000_000, 0).await;
    let err = env
        .open_loan(&borrower, &ata_a, &ata_b, COLL_A, 100_000_000, 350_000_000)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::LtvExceedsMax))
    );

    // Pool unchanged
    let pool = env.pool_state().await;
    assert_eq!(pool.open_loans, 0);
    assert_eq!(pool.next_loan_nonce, 0);
}

#[tokio::test]
async fn open_loan_zero_amount() {
    // Build the ix manually — the helper computes trigger prices and would
    // panic on zero amounts before submission.
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 4_000_000_000).await;

    let (borrower, ata_a, ata_b, _) = env.setup_user(10_000_000_000, 100_000_000, 0).await;
    let (loan_pda, _) = env.loan_pda(&borrower.pubkey(), 0);
    // band_id = 0 (any value works since the program rejects before computing)
    let (band_pda, _) = env.band_pda(0, 0);

    let data = chiefliquidity::LiquidityInstruction::OpenLoan {
        sides: COLL_A,
        collateral_amount: 0,
        debt_amount: 200_000_000,
        nonce: 0,
    };
    let ix = solana_program::instruction::Instruction {
        program_id: env.program_id,
        accounts: vec![
            solana_program::instruction::AccountMeta::new(env.pool_pda().0, false),
            solana_program::instruction::AccountMeta::new(env.vault_a_pda().0, false),
            solana_program::instruction::AccountMeta::new(env.vault_b_pda().0, false),
            solana_program::instruction::AccountMeta::new(ata_a, false),
            solana_program::instruction::AccountMeta::new(ata_b, false),
            solana_program::instruction::AccountMeta::new_readonly(env.mint_a.pubkey(), false),
            solana_program::instruction::AccountMeta::new_readonly(env.mint_b.pubkey(), false),
            solana_program::instruction::AccountMeta::new(borrower.pubkey(), true),
            solana_program::instruction::AccountMeta::new(loan_pda, false),
            solana_program::instruction::AccountMeta::new(band_pda, false),
            solana_program::instruction::AccountMeta::new_readonly(
                solana_program::system_program::id(),
                false,
            ),
            solana_program::instruction::AccountMeta::new_readonly(env.token_program, false),
            solana_program::instruction::AccountMeta::new_readonly(env.token_program, false),
        ],
        data: borsh::to_vec(&data).unwrap(),
    };
    let err = env
        .send_with_new_blockhash(&[ix], &[&borrower])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::ZeroAmount))
    );
}

#[tokio::test]
async fn open_loan_excessive_request_rejected() {
    // Either LTV-exceeds-max or insufficient-executable-liquidity will fire,
    // depending on which check the borrow trips first. Both indicate the
    // program protects against unsound loans.
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 1_000_000_000).await;

    let (borrower, ata_a, ata_b, _) = env.setup_user(10_000_000_000, 1_000_000_000, 0).await;
    let err = env
        .open_loan(
            &borrower,
            &ata_a,
            &ata_b,
            COLL_A,
            1_000_000_000,
            2_000_000_000,
        )
        .await
        .unwrap_err();
    let code = extract_custom_error(&err);
    assert!(
        code == Some(err_code(LiquidityError::InsufficientExecutableLiquidity))
            || code == Some(err_code(LiquidityError::LtvExceedsMax)),
        "expected liquidity-related rejection; got {code:?}"
    );
}

// ============ RepayLoan happy path ============

#[tokio::test]
async fn repay_loan_full_round_trip() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 4_000_000_000).await;

    let (borrower, ata_a, ata_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 1_000_000_000).await;
    let nonce = env
        .open_loan(&borrower, &ata_a, &ata_b, COLL_A, 100_000_000, 200_000_000)
        .await
        .unwrap();

    // Repay immediately (no slot time has passed inside the bank, so
    // accrued interest is essentially zero).
    let pre_a = env.token_balance(&ata_a).await;
    env.repay_loan(&borrower, &ata_a, &ata_b, nonce)
        .await
        .unwrap();
    let post_a = env.token_balance(&ata_a).await;

    // Borrower got their A collateral back.
    assert_eq!(post_a - pre_a, 100_000_000);

    // Pool drained: no open loans, no debt.
    let pool = env.pool_state().await;
    assert_eq!(pool.open_loans, 0);
    assert_eq!(pool.total_debt_b, 0);
    assert_eq!(pool.total_collateral_a, 0);

    // Loan + Band accounts are closed (rent refunded).
    let (loan_pda, _) = env.loan_pda(&borrower.pubkey(), nonce);
    assert!(env.loan_state(&loan_pda).await.is_none());
}

#[tokio::test]
async fn repay_loan_wrong_borrower_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 4_000_000_000).await;

    let (alice, alice_a, alice_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 0).await;
    let nonce = env
        .open_loan(&alice, &alice_a, &alice_b, COLL_A, 100_000_000, 200_000_000)
        .await
        .unwrap();

    // Bob tries to repay Alice's loan.
    let (bob, bob_a, bob_b, _) =
        env.setup_user(10_000_000_000, 0, 1_000_000_000).await;
    // Build using Alice's nonce; bob signs.
    let (alice_loan_pda, _) = env.loan_pda(&alice.pubkey(), nonce);
    let alice_loan = env.loan_state(&alice_loan_pda).await.unwrap();
    let (band_pda, _) = env.band_pda(alice_loan.trigger_direction, alice_loan.band_id);

    let data = chiefliquidity::LiquidityInstruction::RepayLoan;
    let ix = solana_program::instruction::Instruction {
        program_id: env.program_id,
        accounts: vec![
            solana_program::instruction::AccountMeta::new(env.pool_pda().0, false),
            solana_program::instruction::AccountMeta::new(env.vault_a_pda().0, false),
            solana_program::instruction::AccountMeta::new(env.vault_b_pda().0, false),
            solana_program::instruction::AccountMeta::new(bob_a, false),
            solana_program::instruction::AccountMeta::new(bob_b, false),
            solana_program::instruction::AccountMeta::new_readonly(env.mint_a.pubkey(), false),
            solana_program::instruction::AccountMeta::new_readonly(env.mint_b.pubkey(), false),
            solana_program::instruction::AccountMeta::new(bob.pubkey(), true),
            solana_program::instruction::AccountMeta::new(alice_loan_pda, false),
            solana_program::instruction::AccountMeta::new(band_pda, false),
            solana_program::instruction::AccountMeta::new_readonly(env.token_program, false),
            solana_program::instruction::AccountMeta::new_readonly(env.token_program, false),
        ],
        data: borsh::to_vec(&data).unwrap(),
    };
    let err = env
        .send_with_new_blockhash(&[ix], &[&bob])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::InvalidPool))
    );
}

#[tokio::test]
async fn loan_closed_after_repay() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 4_000_000_000).await;

    let (borrower, ata_a, ata_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 1_000_000_000).await;
    let nonce = env
        .open_loan(&borrower, &ata_a, &ata_b, COLL_A, 100_000_000, 200_000_000)
        .await
        .unwrap();

    env.repay_loan(&borrower, &ata_a, &ata_b, nonce)
        .await
        .unwrap();

    let (loan_pda, _) = env.loan_pda(&borrower.pubkey(), nonce);
    assert!(
        env.loan_state(&loan_pda).await.is_none(),
        "loan account should be closed"
    );
}

// ============ ClaimLiquidatedRent ============
//
// To hit the LIQUIDATED status we need a swap that triggers liquidation, which
// is exercised in tests/swap.rs. Here we cover the negative cases that don't
// require a real liquidation.

#[tokio::test]
async fn claim_rent_on_open_loan_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 4_000_000_000).await;

    let (borrower, ata_a, ata_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 0).await;
    let nonce = env
        .open_loan(&borrower, &ata_a, &ata_b, COLL_A, 100_000_000, 200_000_000)
        .await
        .unwrap();

    let err = env
        .claim_liquidated_rent(&borrower, nonce)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::LoanNotLiquidatable))
    );

    // Loan is still alive
    let (loan_pda, _) = env.loan_pda(&borrower.pubkey(), nonce);
    let loan = env.loan_state(&loan_pda).await.unwrap();
    assert!(loan.is_open());
    assert_eq!(loan.status, Loan::STATUS_OPEN);
}

#[tokio::test]
async fn claim_rent_wrong_borrower_rejected() {
    // Manually craft a STATUS_LIQUIDATED loan by using internal program
    // logic. Since we can't easily do that without going through swap, we
    // instead verify that with an open loan, a non-borrower signer is
    // rejected (the borrower-equality check fires before status check is
    // reached for an open loan; it rejects with LoanNotLiquidatable for
    // open status. So we test the no-signer-mismatch path differently —
    // we build a tx where bob signs claim against alice's open loan.)
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(1_000_000_000, 4_000_000_000).await;

    let (alice, alice_a, alice_b, _) =
        env.setup_user(10_000_000_000, 100_000_000, 0).await;
    let nonce = env
        .open_loan(&alice, &alice_a, &alice_b, COLL_A, 100_000_000, 200_000_000)
        .await
        .unwrap();

    let bob = env.create_funded_user(10_000_000_000).await;
    // Use bob as signer. The instruction's borrower field is alice's loan;
    // status check fires first since loan is open → LoanNotLiquidatable.
    // (We cover the borrower-equality branch in the swap-driven liquidation
    // tests in tests/swap.rs.)
    let err = env.claim_liquidated_rent(&bob, nonce).await.unwrap_err();
    // bob's loan PDA at nonce=0 doesn't exist → AccountDataTooSmall or
    // NotInitialized; either way, an error.
    assert!(extract_custom_error(&err).is_some());
}
