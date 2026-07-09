//! Protocol-fee redemption tests (gated on the program upgrade authority) +
//! cross-cutting abuse scenarios.
//!
//! Pools are immutable and authority-less — there is no TransferAuthority or
//! UpdatePoolSettings anymore, so those tests are gone. Fee redemption is
//! gated on the program's upgrade authority, which the harness seeds into an
//! injected ProgramData account as `env.upgrade_authority`.

mod common;

use chiefliquidity::error::LiquidityError;
use common::{err_code, extract_custom_error, TestEnv};
use solana_sdk::signature::Signer;

// ============ ClaimProtocolFees (program-upgrade-authority gated) ============

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

    // The program's upgrade authority (not any pool authority) claims.
    let auth = env.upgrade_authority.insecure_clone();
    let dest_a = env.create_ata(&auth.pubkey(), &env.mint_a.pubkey()).await;
    let dest_b = env.create_ata(&auth.pubkey(), &env.mint_b.pubkey()).await;
    env.claim_protocol_fees(&auth, &dest_a, &dest_b)
        .await
        .unwrap();

    // Tokens landed in the upgrade authority's accounts.
    assert_eq!(env.token_balance(&dest_a).await, expected_a);
    assert_eq!(env.token_balance(&dest_b).await, 0);

    // Pool counters reset.
    let pool_post = env.pool_state().await;
    assert_eq!(pool_post.protocol_fees_a, 0);
    assert_eq!(pool_post.protocol_fees_b, 0);
}

#[tokio::test]
async fn claim_protocol_fees_non_upgrade_authority_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env
        .setup_pool_with_liquidity(1_000_000_000, 4_000_000_000)
        .await;
    let (trader, ta, tb, _) = env.setup_user(10_000_000_000, 100_000_000, 0).await;
    env.swap_full(&trader, &ta, &tb, 100_000_000, 1, true)
        .await
        .unwrap();

    // Anyone who is not the program upgrade authority is rejected — including
    // the pool creator / fee payer (pools grant no authority).
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
