//! Integration tests for `InitializePool`.

mod common;

use chiefliquidity::{
    error::LiquidityError,
    instructions::initialize_pool::{
        INTEREST_BASE_BPS_PER_YEAR, INTEREST_KINK_BPS, INTEREST_SLOPE1_BPS_PER_YEAR,
        INTEREST_SLOPE2_BPS_PER_YEAR, LIQ_RATIO_BPS, MAX_LTV_BPS, PROTOCOL_FEE_BPS, SWAP_FEE_BPS,
    },
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
    // Pools are authority-less: the creator gains no rights.
    assert_eq!(pool.authority, solana_program::pubkey::Pubkey::default());

    // Economics are the fixed program constants, not creator-chosen.
    assert_eq!(pool.swap_fee_bps, SWAP_FEE_BPS);
    assert_eq!(pool.protocol_fee_bps, PROTOCOL_FEE_BPS);
    assert_eq!(pool.liq_ratio_bps, LIQ_RATIO_BPS);
    assert_eq!(pool.max_ltv_bps, MAX_LTV_BPS);
    assert_eq!(pool.interest_base_bps_per_year, INTEREST_BASE_BPS_PER_YEAR);
    assert_eq!(pool.interest_slope1_bps_per_year, INTEREST_SLOPE1_BPS_PER_YEAR);
    assert_eq!(pool.interest_slope2_bps_per_year, INTEREST_SLOPE2_BPS_PER_YEAR);
    assert_eq!(pool.interest_kink_bps, INTEREST_KINK_BPS);

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

    let pool = env.pool_pda().0;
    let vault_a = env.vault_a_pda().0;
    let vault_b = env.vault_b_pda().0;
    let lp_mint = env.lp_mint_pda().0;
    let data = LiquidityInstruction::InitializePool;
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

// Parameter-bounds tests were removed: pool economics are fixed program
// constants baked in by InitializePool (validated at compile time in
// initialize_pool.rs), so there are no caller-supplied params to reject.
