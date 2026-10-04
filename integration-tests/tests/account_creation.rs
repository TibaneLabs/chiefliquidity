//! Integration tests for account-creation hardening:
//!
//! - pre-funded PDAs (pool, vaults, LP mint, band, loan) no longer block
//!   creation (`account_utils::create_pda_account`);
//! - `RepayLoan` leaves an emptied band allocated, and `OpenLoan` re-uses it
//!   (and re-initializes a revived all-zero band in place);
//! - `OpenLoan` accepts any free, client-chosen nonce, but never re-initializes
//!   a revived all-zero Loan;
//! - Token-2022 mints with a *set* `MintCloseAuthority` are rejected.

mod common;

use chiefliquidity::{
    error::LiquidityError,
    state::{bitmap_is_set, Loan, LoanIndexBand, Pool, POOL_DISCRIMINATOR},
    LiquidityInstruction,
};
use common::{err_code, extract_custom_error, TestEnv};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
};
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use spl_token_2022::{
    extension::{ExtensionType, StateWithExtensions},
    instruction as token_ix,
    state::{Account as TokenAccount, Mint},
};

// Sides byte: 0 = CollateralA / DebtB.
const COLL_A: u8 = 0;
const COLL: u64 = 100_000_000;
const DEBT: u64 = 200_000_000;

/// Smallest balance a fresh system account may be funded with (rent-exempt at
/// 0 bytes) — the cheapest possible "pre-funding" griefing transfer.
fn dust() -> u64 {
    Rent::default().minimum_balance(0)
}

async fn prefund(env: &mut TestEnv, target: &Pubkey, lamports: u64) {
    let ix = system_instruction::transfer(&env.payer.pubkey(), target, lamports);
    env.send_with_new_blockhash(&[ix], &[]).await.unwrap();
}

/// Program-owned, all-zero account of `len` bytes: what a closed account looks
/// like after its lamports are topped back up inside the closing transaction.
fn zeroed_program_account(len: usize) -> Account {
    Account {
        lamports: Rent::default().minimum_balance(len),
        data: vec![0u8; len],
        owner: chiefliquidity::id(),
        executable: false,
        rent_epoch: 0,
    }
}

// ============ A1: pre-funded PDAs ============

#[tokio::test]
async fn initialize_pool_succeeds_with_prefunded_pdas() {
    let mut env = TestEnv::new().await;
    let pool = env.pool_pda().0;
    let vault_a = env.vault_a_pda().0;
    let vault_b = env.vault_b_pda().0;
    let lp_mint = env.lp_mint_pda().0;

    // Mix of below-rent and above-rent pre-funding.
    prefund(&mut env, &pool, dust()).await;
    prefund(&mut env, &vault_a, dust() + 12_345).await;
    prefund(&mut env, &vault_b, 1_000_000_000).await;
    prefund(&mut env, &lp_mint, dust()).await;

    env.initialize_pool_default().await;

    let state = env.pool_state().await;
    assert_eq!(state.discriminator, POOL_DISCRIMINATOR);
    let rent = env.banks_client.get_rent().await.unwrap();

    let pool_acc = env.banks_client.get_account(pool).await.unwrap().unwrap();
    assert_eq!(pool_acc.owner, env.program_id);
    assert_eq!(pool_acc.data.len(), Pool::LEN);
    assert!(pool_acc.lamports >= rent.minimum_balance(Pool::LEN));

    for (vault, mint) in [
        (vault_a, env.mint_a.pubkey()),
        (vault_b, env.mint_b.pubkey()),
    ] {
        let acc = env.banks_client.get_account(vault).await.unwrap().unwrap();
        assert_eq!(acc.owner, env.token_program);
        assert!(acc.lamports >= rent.minimum_balance(acc.data.len()));
        let tok = StateWithExtensions::<TokenAccount>::unpack(&acc.data)
            .unwrap()
            .base;
        assert_eq!(tok.owner, pool);
        assert_eq!(tok.mint, mint);
    }
    // Over-funding is kept, not refunded.
    let vb = env
        .banks_client
        .get_account(vault_b)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(vb.lamports, 1_000_000_000);

    let lp = env
        .banks_client
        .get_account(lp_mint)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lp.owner, env.token_program);
    let lp_state = StateWithExtensions::<Mint>::unpack(&lp.data).unwrap().base;
    let auth: Option<Pubkey> = lp_state.mint_authority.into();
    assert_eq!(auth, Some(pool));

    // And the pool is fully usable.
    let (user, ata_a, ata_b, ata_lp) = env
        .setup_user(10_000_000_000, 2_000_000_000, 8_000_000_000)
        .await;
    let ix = env.ix_add_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        1_000_000_000,
        4_000_000_000,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&user]).await.unwrap();
}

#[tokio::test]
async fn open_loan_succeeds_with_prefunded_band_and_loan_pdas() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    let (borrower, ata_a, ata_b, _) = env.setup_user(10_000_000_000, COLL, 0).await;

    let nonce = env.pool_state().await.next_loan_nonce;
    let (loan_pda, _) = env.loan_pda(&borrower.pubkey(), nonce);
    let (band_pda, band_id, dir) = env.loan_band(COLL_A, COLL, DEBT);
    prefund(&mut env, &loan_pda, dust()).await;
    prefund(&mut env, &band_pda, dust() + 7).await;

    env.open_loan_with_nonce(&borrower, &ata_a, &ata_b, COLL_A, COLL, DEBT, nonce)
        .await
        .unwrap();

    let loan = env.loan_state(&loan_pda).await.unwrap();
    assert!(loan.is_initialized() && loan.is_open());
    assert_eq!(loan.nonce, nonce);
    let band = env.band_state(dir, band_id).await.unwrap();
    assert!(band.is_initialized());
    assert_eq!(band.count, 1);

    let rent = env.banks_client.get_rent().await.unwrap();
    let loan_acc = env
        .banks_client
        .get_account(loan_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loan_acc.owner, env.program_id);
    assert!(loan_acc.lamports >= rent.minimum_balance(Loan::LEN));
    let band_acc = env
        .banks_client
        .get_account(band_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(band_acc.owner, env.program_id);
    assert!(band_acc.lamports >= rent.minimum_balance(LoanIndexBand::LEN));
}

// ============ A2: bands are never closed ============

#[tokio::test]
async fn repay_last_loan_keeps_band_and_band_is_reusable() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    let (borrower, ata_a, ata_b, _) = env.setup_user(10_000_000_000, COLL, 1_000_000_000).await;
    let (band_pda, band_id, dir) = env.loan_band(COLL_A, COLL, DEBT);

    let nonce = env
        .open_loan(&borrower, &ata_a, &ata_b, COLL_A, COLL, DEBT)
        .await
        .unwrap();
    let band_lamports = env
        .banks_client
        .get_account(band_pda)
        .await
        .unwrap()
        .unwrap()
        .lamports;
    env.repay_loan(&borrower, &ata_a, &ata_b, nonce)
        .await
        .unwrap();

    // Band stays allocated with count 0, rent untouched; bitmap bit cleared.
    let band = env.band_state(dir, band_id).await.expect("band kept");
    assert!(band.is_initialized());
    assert_eq!(band.count, 0);
    let band_acc = env
        .banks_client
        .get_account(band_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(band_acc.lamports, band_lamports);
    let pool = env.pool_state().await;
    assert!(!bitmap_is_set(pool.band_bitmap(dir).unwrap(), band_id));

    // A new loan in the same band re-populates it (and re-sets the bit).
    env.open_loan(&borrower, &ata_a, &ata_b, COLL_A, COLL, DEBT)
        .await
        .unwrap();
    let band = env.band_state(dir, band_id).await.unwrap();
    assert_eq!(band.count, 1);
    let pool = env.pool_state().await;
    assert!(bitmap_is_set(pool.band_bitmap(dir).unwrap(), band_id));
}

#[tokio::test]
async fn open_loan_reinitializes_revived_zeroed_band() {
    // A band closed by a pre-fix RepayLoan and topped back up in the same tx:
    // program-owned, 88 zero bytes. It must be usable, not BandMismatch forever.
    let mut env = TestEnv::new_with_accounts(|mint_a, mint_b| {
        let program_id = chiefliquidity::id();
        let (pool, _) = Pool::derive_pda(mint_a, mint_b, &program_id);
        let (band_id, dir) = band_for(COLL_A, COLL, DEBT);
        let (band, _) = LoanIndexBand::derive_pda(&pool, dir, band_id, &program_id);
        vec![(band, zeroed_program_account(LoanIndexBand::LEN))]
    })
    .await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    let (borrower, ata_a, ata_b, _) = env.setup_user(10_000_000_000, COLL, 0).await;
    let (band_pda, band_id, dir) = env.loan_band(COLL_A, COLL, DEBT);
    assert!(env
        .band_state(dir, band_id)
        .await
        .is_some_and(|b| !b.is_initialized()));

    env.open_loan(&borrower, &ata_a, &ata_b, COLL_A, COLL, DEBT)
        .await
        .unwrap();

    let band = env.band_state(dir, band_id).await.unwrap();
    assert!(band.is_initialized());
    assert_eq!(band.pool, env.pool_pda().0);
    assert_eq!(band.band_id, band_id);
    assert_eq!(band.direction, dir);
    assert_eq!(band.bump, env.band_pda(dir, band_id).1);
    assert_eq!(band.count, 1);
    let band_acc = env
        .banks_client
        .get_account(band_pda)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(band_acc.owner, env.program_id);
    let pool = env.pool_state().await;
    assert!(bitmap_is_set(pool.band_bitmap(dir).unwrap(), band_id));
}

fn band_for(sides: u8, coll: u64, debt: u64) -> (u32, u8) {
    let sides = chiefliquidity::math::LoanSides::from_u8(sides).unwrap();
    let (trigger, dir) = chiefliquidity::math::recompute_trigger(
        sides,
        coll as u128,
        debt as u128,
        chiefliquidity::instructions::initialize_pool::LIQ_RATIO_BPS,
    )
    .unwrap();
    (
        chiefliquidity::math::band_id_for_trigger(trigger).unwrap(),
        dir as u8,
    )
}

// ============ A3: client-chosen nonces ============

#[tokio::test]
async fn open_loan_with_arbitrary_nonce() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    let (alice, alice_a, alice_b, _) = env.setup_user(10_000_000_000, 3 * COLL, 0).await;
    let (bob, bob_a, bob_b, _) = env.setup_user(10_000_000_000, COLL, 0).await;

    // A non-counter nonce works.
    let n1 = 0xDEAD_BEEF_u64;
    env.open_loan_with_nonce(&alice, &alice_a, &alice_b, COLL_A, COLL, DEBT, n1)
        .await
        .unwrap();
    let (loan1, _) = env.loan_pda(&alice.pubkey(), n1);
    assert_eq!(env.loan_state(&loan1).await.unwrap().nonce, n1);

    // Two borrowers using the same nonce concurrently don't collide (the PDA
    // is per-borrower), and neither depends on the pool counter.
    env.open_loan_with_nonce(&bob, &bob_a, &bob_b, COLL_A, COLL, DEBT, n1)
        .await
        .unwrap();

    // A nonce whose Loan PDA is already live is rejected.
    let err = env
        .open_loan_with_nonce(&alice, &alice_a, &alice_b, COLL_A, COLL, DEBT + 1, n1)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::AlreadyInitialized))
    );

    // The counter is informational: bumped on every open, and passing it
    // (classic client behaviour) still works.
    assert_eq!(env.pool_state().await.next_loan_nonce, 2);
    let n2 = env
        .open_loan(&alice, &alice_a, &alice_b, COLL_A, COLL, DEBT)
        .await
        .unwrap();
    assert_eq!(n2, 2);
    let pool = env.pool_state().await;
    assert_eq!(pool.next_loan_nonce, 3);
    assert_eq!(pool.open_loans, 3);
}

#[tokio::test]
async fn open_loan_rejects_revived_zeroed_loan_pda() {
    // A previously-closed Loan revived as a program-owned, all-zero account
    // must NOT be re-initialized; the borrower just picks another nonce.
    let borrower = Keypair::new();
    let borrower_key = borrower.pubkey();
    let revived_nonce = 7u64;
    let mut env = TestEnv::new_with_accounts(|mint_a, mint_b| {
        let program_id = chiefliquidity::id();
        let (pool, _) = Pool::derive_pda(mint_a, mint_b, &program_id);
        let (loan, _) = Loan::derive_pda(&pool, &borrower_key, revived_nonce, &program_id);
        vec![(loan, zeroed_program_account(Loan::LEN))]
    })
    .await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    prefund(&mut env, &borrower_key, 10_000_000_000).await;
    let ata_a = env
        .fund_token(&borrower_key, &env.mint_a.pubkey(), COLL)
        .await;
    let ata_b = env.create_ata(&borrower_key, &env.mint_b.pubkey()).await;

    let err = env
        .open_loan_with_nonce(&borrower, &ata_a, &ata_b, COLL_A, COLL, DEBT, revived_nonce)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::AlreadyInitialized))
    );

    env.open_loan_with_nonce(
        &borrower,
        &ata_a,
        &ata_b,
        COLL_A,
        COLL,
        DEBT,
        revived_nonce + 1,
    )
    .await
    .unwrap();
}

// ============ A4: MintCloseAuthority ============

/// Create a Token-2022 mint carrying the `MintCloseAuthority` extension.
async fn create_mint_with_close_authority(
    env: &mut TestEnv,
    close_authority: Option<Pubkey>,
) -> Keypair {
    let mint = Keypair::new();
    let space =
        ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::MintCloseAuthority])
            .unwrap();
    let rent = env.banks_client.get_rent().await.unwrap();
    let ixs = [
        system_instruction::create_account(
            &env.payer.pubkey(),
            &mint.pubkey(),
            rent.minimum_balance(space),
            space as u64,
            &spl_token_2022::id(),
        ),
        token_ix::initialize_mint_close_authority(
            &spl_token_2022::id(),
            &mint.pubkey(),
            close_authority.as_ref(),
        )
        .unwrap(),
        token_ix::initialize_mint2(
            &spl_token_2022::id(),
            &mint.pubkey(),
            &env.payer.pubkey(),
            None,
            6,
        )
        .unwrap(),
    ];
    env.refresh_blockhash().await;
    let mut tx = Transaction::new_with_payer(&ixs, Some(&env.payer.pubkey()));
    tx.sign(&[&env.payer, &mint], env.last_blockhash);
    env.banks_client.process_transaction(tx).await.unwrap();
    mint
}

/// InitializePool for `other` paired with `env.mint_a` (sorted).
fn ix_initialize_pool_for(env: &TestEnv, other: &Pubkey) -> Instruction {
    let program_id = env.program_id;
    let (lo, hi) = if env.mint_a.pubkey() < *other {
        (env.mint_a.pubkey(), *other)
    } else {
        (*other, env.mint_a.pubkey())
    };
    let (pool, _) = Pool::derive_pda(&lo, &hi, &program_id);
    Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(lo, false),
            AccountMeta::new_readonly(hi, false),
            AccountMeta::new(Pool::derive_vault_a_pda(&pool, &program_id).0, false),
            AccountMeta::new(Pool::derive_vault_b_pda(&pool, &program_id).0, false),
            AccountMeta::new(Pool::derive_lp_mint_pda(&pool, &program_id).0, false),
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(solana_program::system_program::id(), false),
            AccountMeta::new_readonly(spl_token_2022::id(), false),
            AccountMeta::new_readonly(spl_token_2022::id(), false),
            AccountMeta::new_readonly(solana_program::sysvar::rent::id(), false),
        ],
        data: borsh::to_vec(&LiquidityInstruction::InitializePool).unwrap(),
    }
}

#[tokio::test]
async fn rejects_mint_with_set_close_authority() {
    let mut env = TestEnv::new().await;
    let closer = Keypair::new().pubkey();
    let mint = create_mint_with_close_authority(&mut env, Some(closer)).await;

    let ix = ix_initialize_pool_for(&env, &mint.pubkey());
    let err = env.send_with_new_blockhash(&[ix], &[]).await.unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::UnsupportedMintExtension))
    );
}

#[tokio::test]
async fn accepts_mint_with_unset_close_authority() {
    let mut env = TestEnv::new().await;
    let mint = create_mint_with_close_authority(&mut env, None).await;

    let ix = ix_initialize_pool_for(&env, &mint.pubkey());
    env.send_with_new_blockhash(&[ix], &[]).await.unwrap();
}
