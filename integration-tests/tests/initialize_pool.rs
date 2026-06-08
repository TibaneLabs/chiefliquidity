//! Integration tests for `InitializePool`.

mod common;

use chiefliquidity::{
    error::LiquidityError,
    math::WAD,
    state::POOL_DISCRIMINATOR,
    LiquidityInstruction,
};
use common::{err_code, extract_custom_error, PoolParams, TestEnv};
use solana_program::{
    instruction::{AccountMeta, Instruction},
};
use solana_sdk::signature::Signer;
use spl_token_2022::{
    extension::StateWithExtensions,
    state::{Account as TokenAccount, Mint},
};

#[tokio::test]
async fn happy_path_creates_all_pdas_and_persists_pool() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;

    let pool = env.pool_state().await;
    assert_eq!(pool.discriminator, POOL_DISCRIMINATOR);
    assert_eq!(pool.mint_a, env.mint_a.pubkey());
    assert_eq!(pool.mint_b, env.mint_b.pubkey());
    assert_eq!(pool.vault_a, env.vault_a_pda().0);
    assert_eq!(pool.vault_b, env.vault_b_pda().0);
    assert_eq!(pool.lp_mint, env.lp_mint_pda().0);
    assert_eq!(pool.authority, env.payer.pubkey());

    // Reserves and counters at zero
    assert_eq!(pool.total_debt_a, 0);
    assert_eq!(pool.total_debt_b, 0);
    assert_eq!(pool.total_collateral_a, 0);
    assert_eq!(pool.total_collateral_b, 0);
    assert_eq!(pool.open_loans, 0);
    assert_eq!(pool.next_loan_nonce, 0);
    assert_eq!(pool.protocol_fees_a, 0);
    assert_eq!(pool.protocol_fees_b, 0);

    // Borrow indexes start at WAD = 1.0
    assert_eq!(pool.borrow_index_a_wad, WAD);
    assert_eq!(pool.borrow_index_b_wad, WAD);

    // Bitmaps start empty
    assert!(pool.band_bitmap_fall.iter().all(|&b| b == 0));
    assert!(pool.band_bitmap_rise.iter().all(|&b| b == 0));

    // Vault A is owned by token program, has 0 amount, has Pool as owner
    let vault_a_acc = env
        .banks_client
        .get_account(env.vault_a_pda().0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(vault_a_acc.owner, env.token_program);
    let vault_a_state = StateWithExtensions::<TokenAccount>::unpack(&vault_a_acc.data).unwrap().base;
    assert_eq!(vault_a_state.amount, 0);
    assert_eq!(vault_a_state.owner, env.pool_pda().0);
    assert_eq!(vault_a_state.mint, env.mint_a.pubkey());

    // LP mint exists with mint_authority = pool PDA, supply = 0
    let lp_acc = env
        .banks_client
        .get_account(env.lp_mint_pda().0)
        .await
        .unwrap()
        .unwrap();
    let lp_mint = StateWithExtensions::<Mint>::unpack(&lp_acc.data).unwrap().base;
    assert_eq!(lp_mint.supply, 0);
    let auth: Option<solana_program::pubkey::Pubkey> = lp_mint.mint_authority.into();
    assert_eq!(auth, Some(env.pool_pda().0));
}

#[tokio::test]
async fn rejects_reinitialize() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;

    let ix = env.ix_initialize_pool(&PoolParams::default());
    let err = env.send_with_new_blockhash(&[ix], &[]).await.unwrap_err();
    let code = extract_custom_error(&err);
    assert_eq!(
        code,
        Some(err_code(LiquidityError::AlreadyInitialized)),
        "expected AlreadyInitialized; got {code:?}"
    );
}

#[tokio::test]
async fn rejects_unsorted_mints() {
    // Build the instruction by hand with mint_a/mint_b deliberately swapped.
    let mut env = TestEnv::new().await;
    let params = PoolParams::default();

    let pool = env.pool_pda().0;
    let vault_a = env.vault_a_pda().0;
    let vault_b = env.vault_b_pda().0;
    let lp_mint = env.lp_mint_pda().0;
    let data = LiquidityInstruction::InitializePool {
        swap_fee_bps: params.swap_fee_bps,
        protocol_fee_bps: params.protocol_fee_bps,
        liq_ratio_bps: params.liq_ratio_bps,
        max_ltv_bps: params.max_ltv_bps,
        interest_base_bps_per_year: params.interest_base_bps_per_year,
        interest_slope1_bps_per_year: params.interest_slope1_bps_per_year,
        interest_slope2_bps_per_year: params.interest_slope2_bps_per_year,
        interest_kink_bps: params.interest_kink_bps,
    };
    // Pass mint_b first, mint_a second — should be rejected.
    let ix = Instruction {
        program_id: env.program_id,
        accounts: vec![
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(env.mint_b.pubkey(), false),
            AccountMeta::new_readonly(env.mint_a.pubkey(), false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
            AccountMeta::new(lp_mint, false),
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new_readonly(solana_program::system_program::id(), false),
            AccountMeta::new_readonly(env.token_program, false),
            AccountMeta::new_readonly(solana_program::sysvar::rent::id(), false),
        ],
        data: borsh::to_vec(&data).unwrap(),
    };
    let err = env.send(&[ix], &[]).await.unwrap_err();
    let code = extract_custom_error(&err);
    // Either MintsNotSorted (the explicit check) or InvalidPDA (because the
    // pool PDA was derived from sorted mints; the supplied accounts use the
    // wrong order). Both indicate the protection is doing its job.
    assert!(
        code == Some(err_code(LiquidityError::MintsNotSorted))
            || code == Some(err_code(LiquidityError::InvalidPDA)),
        "expected MintsNotSorted or InvalidPDA; got {code:?}"
    );
}

#[tokio::test]
async fn rejects_swap_fee_above_max() {
    let mut env = TestEnv::new().await;
    let mut p = PoolParams::default();
    p.swap_fee_bps = 5_000; // 50% > MAX_SWAP_FEE_BPS = 10%
    let ix = env.ix_initialize_pool(&p);
    let err = env.send(&[ix], &[]).await.unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::SettingExceedsMaximum))
    );
}

#[tokio::test]
async fn rejects_protocol_fee_above_swap_fee() {
    let mut env = TestEnv::new().await;
    let mut p = PoolParams::default();
    p.swap_fee_bps = 30;
    p.protocol_fee_bps = 50; // > swap fee
    let ix = env.ix_initialize_pool(&p);
    let err = env.send(&[ix], &[]).await.unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::SettingExceedsMaximum))
    );
}

#[tokio::test]
async fn rejects_liq_ratio_below_min() {
    let mut env = TestEnv::new().await;
    let mut p = PoolParams::default();
    p.liq_ratio_bps = 9_000; // < MIN 10100
    let ix = env.ix_initialize_pool(&p);
    let err = env.send(&[ix], &[]).await.unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::SettingExceedsMaximum))
    );
}

#[tokio::test]
async fn rejects_max_ltv_unsafe_vs_liq_ratio() {
    let mut env = TestEnv::new().await;
    let mut p = PoolParams::default();
    // max_ltv must be < BPS_DENOM^2 / liq_ratio. With liq_ratio=11000,
    // upper bound = 100_000_000 / 11_000 ≈ 9090. Pick 9500 → reject.
    p.liq_ratio_bps = 11_000;
    p.max_ltv_bps = 9_500;
    let ix = env.ix_initialize_pool(&p);
    let err = env.send(&[ix], &[]).await.unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::SettingExceedsMaximum))
    );
}

#[tokio::test]
async fn rejects_kink_at_zero_or_full() {
    let mut env = TestEnv::new().await;
    let mut p = PoolParams::default();
    p.interest_kink_bps = 0;
    let ix = env.ix_initialize_pool(&p);
    let err = env.send(&[ix], &[]).await.unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::SettingExceedsMaximum))
    );

    let mut env2 = TestEnv::new().await;
    let mut p2 = PoolParams::default();
    p2.interest_kink_bps = 10_000; // 100% — must be < MAX_KINK_BPS
    let ix2 = env2.ix_initialize_pool(&p2);
    let err2 = env2.send(&[ix2], &[]).await.unwrap_err();
    assert_eq!(
        extract_custom_error(&err2),
        Some(err_code(LiquidityError::SettingExceedsMaximum))
    );
}
