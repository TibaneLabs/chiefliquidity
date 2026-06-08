//! Admin instruction tests + cross-cutting abuse scenarios.

mod common;

use chiefliquidity::error::LiquidityError;
use common::{err_code, extract_custom_error, PoolParams, TestEnv};
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

// ============ TransferAuthority ============

#[tokio::test]
async fn transfer_authority_happy() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;

    let new_auth = Keypair::new();
    // Payer is the current authority (set by initialize_pool).
    // Need to clone payer because env.payer is borrowed by send.
    let payer_clone = env.payer.insecure_clone();
    env.transfer_authority(&payer_clone, new_auth.pubkey())
        .await
        .unwrap();

    let pool = env.pool_state().await;
    assert_eq!(pool.authority, new_auth.pubkey());
    assert!(!pool.is_authority_renounced());
}

#[tokio::test]
async fn transfer_authority_non_authority_rejected() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;

    let attacker = env.create_funded_user(10_000_000_000).await;
    let err = env
        .transfer_authority(&attacker, attacker.pubkey())
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::InvalidAuthority))
    );

    // Authority unchanged
    let pool = env.pool_state().await;
    assert_eq!(pool.authority, env.payer.pubkey());
}

#[tokio::test]
async fn transfer_authority_renounce_then_blocked() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;

    let payer = env.payer.insecure_clone();
    env.transfer_authority(&payer, Pubkey::default())
        .await
        .unwrap();

    let pool = env.pool_state().await;
    assert!(pool.is_authority_renounced());

    // Try to transfer again — renounced.
    let err = env
        .transfer_authority(&payer, payer.pubkey())
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::AuthorityRenounced))
    );
}

// ============ UpdatePoolSettings ============

#[tokio::test]
async fn update_pool_settings_happy() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;
    let payer = env.payer.insecure_clone();

    let mut new_params = PoolParams::default();
    new_params.swap_fee_bps = 50; // change from 30 → 50
    new_params.interest_kink_bps = 9000; // change from 8000 → 9000
    env.update_pool_settings(&payer, &new_params).await.unwrap();

    let pool = env.pool_state().await;
    assert_eq!(pool.swap_fee_bps, 50);
    assert_eq!(pool.interest_kink_bps, 9000);
}

#[tokio::test]
async fn update_pool_settings_non_authority_rejected() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;

    let attacker = env.create_funded_user(10_000_000_000).await;
    let mut params = PoolParams::default();
    params.swap_fee_bps = 999;
    let err = env
        .update_pool_settings(&attacker, &params)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::InvalidAuthority))
    );
}

#[tokio::test]
async fn update_pool_settings_bad_params_rejected() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;
    let payer = env.payer.insecure_clone();

    let mut params = PoolParams::default();
    params.swap_fee_bps = 5_000; // 50% > MAX 10%
    let err = env.update_pool_settings(&payer, &params).await.unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::SettingExceedsMaximum))
    );
}

#[tokio::test]
async fn update_after_renounce_rejected() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;
    let payer = env.payer.insecure_clone();

    env.transfer_authority(&payer, Pubkey::default())
        .await
        .unwrap();

    let err = env
        .update_pool_settings(&payer, &PoolParams::default())
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::AuthorityRenounced))
    );
}

// ============ ClaimProtocolFees ============

#[tokio::test]
async fn claim_protocol_fees_happy() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    // Generate fees by trading.
    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 100_000_000, 0).await;
    env.swap_full(&trader, &ta, &tb, 100_000_000, 1, true)
        .await
        .unwrap();

    let pool = env.pool_state().await;
    assert!(pool.protocol_fees_a > 0);
    let expected_a = pool.protocol_fees_a;

    // Authority claims.
    let payer = env.payer.insecure_clone();
    let dest_a = env.create_ata(&payer.pubkey(), &env.mint_a.pubkey()).await;
    let dest_b = env.create_ata(&payer.pubkey(), &env.mint_b.pubkey()).await;
    env.claim_protocol_fees(&payer, &dest_a, &dest_b)
        .await
        .unwrap();

    // Tokens landed in authority's accounts.
    assert_eq!(env.token_balance(&dest_a).await, expected_a);
    assert_eq!(env.token_balance(&dest_b).await, 0);

    // Pool counters reset.
    let pool_post = env.pool_state().await;
    assert_eq!(pool_post.protocol_fees_a, 0);
    assert_eq!(pool_post.protocol_fees_b, 0);
}

#[tokio::test]
async fn claim_protocol_fees_non_authority_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 100_000_000, 0).await;
    env.swap_full(&trader, &ta, &tb, 100_000_000, 1, true)
        .await
        .unwrap();

    let attacker = env.create_funded_user(10_000_000_000).await;
    let dest_a = env.create_ata(&attacker.pubkey(), &env.mint_a.pubkey()).await;
    let dest_b = env.create_ata(&attacker.pubkey(), &env.mint_b.pubkey()).await;
    let err = env
        .claim_protocol_fees(&attacker, &dest_a, &dest_b)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::InvalidAuthority))
    );
}

#[tokio::test]
async fn claim_protocol_fees_after_renounce_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    let payer = env.payer.insecure_clone();
    env.transfer_authority(&payer, Pubkey::default())
        .await
        .unwrap();

    let dest_a = env.create_ata(&payer.pubkey(), &env.mint_a.pubkey()).await;
    let dest_b = env.create_ata(&payer.pubkey(), &env.mint_b.pubkey()).await;
    let err = env
        .claim_protocol_fees(&payer, &dest_a, &dest_b)
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::AuthorityRenounced))
    );
}

// ============ Cross-cutting abuse: substitute pool with attacker-owned account ============

#[tokio::test]
async fn substituted_pool_account_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    // Try to AddLiquidity but pass a system-owned account in place of the pool.
    let attacker_pool = env.create_funded_user(1_000_000).await; // a SystemAccount, not program-owned
    let (user, ata_a, ata_b, ata_lp) =
        env.setup_user(10_000_000_000, 50_000_000, 200_000_000).await;

    let mut ix = env.ix_add_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        50_000_000,
        200_000_000,
        1,
    );
    // Replace the pool account (slot 0) with our system-owned attacker account.
    ix.accounts[0].pubkey = attacker_pool.pubkey();
    let err = env
        .send_with_new_blockhash(&[ix], &[&user])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::InvalidAccountOwner))
    );
}

#[tokio::test]
async fn swap_with_wrong_vault_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;

    // Try to swap, but swap the order of vault_a and vault_b in the accounts.
    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 10_000_000, 0).await;
    let pool_pda = env.pool_pda().0;
    let v_a = env.vault_a_pda().0;
    let v_b = env.vault_b_pda().0;

    let data = chiefliquidity::LiquidityInstruction::Swap {
        amount_in: 10_000_000,
        min_out: 1,
        a_to_b: true,
        band_boundary: 0,
        band_loan_counts: vec![],
    };
    let ix = solana_program::instruction::Instruction {
        program_id: env.program_id,
        accounts: vec![
            solana_program::instruction::AccountMeta::new(pool_pda, false),
            solana_program::instruction::AccountMeta::new(v_b, false), // SWAPPED
            solana_program::instruction::AccountMeta::new(v_a, false), // SWAPPED
            solana_program::instruction::AccountMeta::new(ta, false),
            solana_program::instruction::AccountMeta::new(tb, false),
            solana_program::instruction::AccountMeta::new_readonly(env.mint_a.pubkey(), false),
            solana_program::instruction::AccountMeta::new_readonly(env.mint_b.pubkey(), false),
            solana_program::instruction::AccountMeta::new_readonly(trader.pubkey(), true),
            solana_program::instruction::AccountMeta::new_readonly(env.token_program, false),
        ],
        data: borsh::to_vec(&data).unwrap(),
    };
    let err = env
        .send_with_new_blockhash(&[ix], &[&trader])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::InvalidPool))
    );
}
