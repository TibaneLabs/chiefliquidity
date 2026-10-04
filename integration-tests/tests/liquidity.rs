//! Integration tests for `AddLiquidity` and `RemoveLiquidity`.

mod common;

use chiefliquidity::{error::LiquidityError, instructions::add_liquidity::MINIMUM_LIQUIDITY};
use common::{err_code, extract_custom_error, TestEnv};
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use solana_sdk::signature::Signer;

// ============ AddLiquidity ============

#[tokio::test]
async fn add_liquidity_first_deposit_mints_sqrt() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;

    // First depositor: 100M of A, 400M of B → LP = sqrt(100M * 400M) = 200M.
    let (user, ata_a, ata_b, ata_lp) =
        env.setup_user(10_000_000_000, 200_000_000, 800_000_000).await;
    let ix = env.ix_add_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        100_000_000,
        400_000_000,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&user]).await.unwrap();

    let lp_balance = env.token_balance(&ata_lp).await;
    assert_eq!(lp_balance, 200_000_000);

    let vault_a_balance = env.token_balance(&env.vault_a_pda().0).await;
    let vault_b_balance = env.token_balance(&env.vault_b_pda().0).await;
    assert_eq!(vault_a_balance, 100_000_000);
    assert_eq!(vault_b_balance, 400_000_000);

    // Pool unchanged in debt/collateral counters.
    let pool = env.pool_state().await;
    assert_eq!(pool.total_debt_a, 0);
    assert_eq!(pool.total_collateral_a, 0);
}

#[tokio::test]
async fn add_liquidity_second_deposit_proportional() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;
    // After: vault_a=100M, vault_b=400M, lp_supply=sqrt(100M*400M)=200M.

    // Second depositor adds in the same ratio (1:4): 50M A and 200M B.
    let (user2, ata_a, ata_b, ata_lp) =
        env.setup_user(10_000_000_000, 100_000_000, 400_000_000).await;
    let ix = env.ix_add_liquidity(
        &user2.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        50_000_000,
        200_000_000,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&user2]).await.unwrap();

    // Should mint LP proportional: 50M / 100M = 50% of supply → 100M LP.
    let lp_supply = env.mint_supply(&env.lp_mint_pda().0).await;
    assert_eq!(lp_supply, 200_000_000 + 100_000_000);
    let lp_balance = env.token_balance(&ata_lp).await;
    assert_eq!(lp_balance, 100_000_000);
}

#[tokio::test]
async fn add_liquidity_excess_b_is_clipped() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;

    // User offers 50M A + 1B B. Ratio is 1:4, so 50M A pairs with 200M B.
    // The extra 800M B should NOT be transferred. Caller-side balance proves it.
    let (user2, ata_a, ata_b, ata_lp) =
        env.setup_user(10_000_000_000, 100_000_000, 1_000_000_000).await;
    let ix = env.ix_add_liquidity(
        &user2.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        50_000_000,
        1_000_000_000,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&user2]).await.unwrap();

    let user_a_left = env.token_balance(&ata_a).await;
    let user_b_left = env.token_balance(&ata_b).await;
    assert_eq!(user_a_left, 100_000_000 - 50_000_000);
    assert_eq!(user_b_left, 1_000_000_000 - 200_000_000);
}

#[tokio::test]
async fn add_liquidity_slippage_breach() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;

    let (user2, ata_a, ata_b, ata_lp) =
        env.setup_user(10_000_000_000, 100_000_000, 400_000_000).await;
    // Demand min_lp_out = 1B. Actual mint will be ~100M. Should revert.
    let ix = env.ix_add_liquidity(
        &user2.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        50_000_000,
        200_000_000,
        1_000_000_000,
    );
    let err = env
        .send_with_new_blockhash(&[ix], &[&user2])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::SlippageExceeded))
    );
}

#[tokio::test]
async fn add_liquidity_below_min_first_deposit() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;

    let (user, ata_a, ata_b, ata_lp) =
        env.setup_user(10_000_000_000, 1_000_000, 1_000_000).await;
    // 100 < MIN_FIRST_DEPOSIT (1_000_000) — should revert
    let ix = env.ix_add_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        100,
        100,
        1,
    );
    let err = env
        .send_with_new_blockhash(&[ix], &[&user])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::ZeroAmount))
    );
}

#[tokio::test]
async fn add_liquidity_without_signer_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;

    let (user2, ata_a, ata_b, ata_lp) =
        env.setup_user(10_000_000_000, 100_000_000, 400_000_000).await;
    let mut ix = env.ix_add_liquidity(
        &user2.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        50_000_000,
        200_000_000,
        1,
    );
    // Strip signer flag from user account; do NOT pass user keypair to
    // signers (so the framework doesn't refuse with KeypairPubkeyMismatch).
    let user_idx = ix.accounts.iter().position(|a| a.pubkey == user2.pubkey()).unwrap();
    ix.accounts[user_idx].is_signer = false;
    let err = env
        .send_with_new_blockhash(&[ix], &[])
        .await
        .unwrap_err();
    // Either MissingRequiredSigner from us, or a token program transfer
    // failure since the SPL transfer needs user as signer too.
    let code = extract_custom_error(&err);
    assert!(
        code == Some(err_code(LiquidityError::MissingRequiredSigner)) || code.is_some(),
        "expected error; got {code:?}"
    );
}

#[tokio::test]
async fn add_liquidity_wrong_vault_rejected() {
    let mut env = TestEnv::new().await;
    let _ = env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;

    let (user2, ata_a, ata_b, ata_lp) =
        env.setup_user(10_000_000_000, 100_000_000, 400_000_000).await;
    let mut ix = env.ix_add_liquidity(
        &user2.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        50_000_000,
        200_000_000,
        1,
    );
    // Swap vault A and vault B — pool's stored keys won't match.
    let v_a_idx = ix.accounts.iter().position(|a| a.pubkey == env.vault_a_pda().0).unwrap();
    let v_b_idx = ix.accounts.iter().position(|a| a.pubkey == env.vault_b_pda().0).unwrap();
    ix.accounts.swap(v_a_idx, v_b_idx);
    let err = env
        .send_with_new_blockhash(&[ix], &[&user2])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::InvalidPool))
    );
}

// ============ RemoveLiquidity ============

#[tokio::test]
async fn remove_liquidity_full_round_trip() {
    let mut env = TestEnv::new().await;
    let (user, ata_a, ata_b, ata_lp) =
        env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;
    let lp_owned = env.token_balance(&ata_lp).await;
    assert_eq!(lp_owned, 200_000_000);

    let pre_vault_a = env.token_balance(&env.vault_a_pda().0).await;
    let pre_vault_b = env.token_balance(&env.vault_b_pda().0).await;

    // Burn all LP — get back proportional A and B.
    let ix = env.ix_remove_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        lp_owned,
        1,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&user]).await.unwrap();

    let post_vault_a = env.token_balance(&env.vault_a_pda().0).await;
    let post_vault_b = env.token_balance(&env.vault_b_pda().0).await;
    assert_eq!(post_vault_a, 0);
    assert_eq!(post_vault_b, 0);

    let user_a = env.token_balance(&ata_a).await;
    let user_b = env.token_balance(&ata_b).await;
    assert_eq!(user_a, 100_000_000 + pre_vault_a);
    assert_eq!(user_b, 400_000_000 + pre_vault_b);
    assert_eq!(env.token_balance(&ata_lp).await, 0);
}

#[tokio::test]
async fn remove_liquidity_partial() {
    let mut env = TestEnv::new().await;
    let (user, ata_a, ata_b, ata_lp) =
        env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;
    let lp_owned = env.token_balance(&ata_lp).await;

    // Burn 25% — expect ~25M A and ~100M B back.
    let burn = lp_owned / 4;
    let ix = env.ix_remove_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        burn,
        24_000_000,
        96_000_000,
    );
    env.send_with_new_blockhash(&[ix], &[&user]).await.unwrap();

    assert_eq!(env.token_balance(&ata_lp).await, lp_owned - burn);
    assert_eq!(env.token_balance(&env.vault_a_pda().0).await, 75_000_000);
    assert_eq!(env.token_balance(&env.vault_b_pda().0).await, 300_000_000);
}

#[tokio::test]
async fn remove_liquidity_slippage_breach() {
    let mut env = TestEnv::new().await;
    let (user, ata_a, ata_b, ata_lp) =
        env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;
    let lp_owned = env.token_balance(&ata_lp).await;

    // Demand way more A than possible.
    let ix = env.ix_remove_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        lp_owned / 4,
        1_000_000_000,
        1,
    );
    let err = env
        .send_with_new_blockhash(&[ix], &[&user])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::SlippageExceeded))
    );
}

#[tokio::test]
async fn remove_liquidity_more_than_supply_rejected() {
    let mut env = TestEnv::new().await;
    let (user, ata_a, ata_b, ata_lp) =
        env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;

    // Try to burn 10x the LP supply.
    let ix = env.ix_remove_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        2_000_000_000,
        1,
        1,
    );
    let err = env
        .send_with_new_blockhash(&[ix], &[&user])
        .await
        .unwrap_err();
    let code = extract_custom_error(&err);
    assert_eq!(
        code,
        Some(err_code(LiquidityError::MathUnderflow)),
        "expected MathUnderflow; got {code:?}"
    );
}

// ============ Share-inflation hardening (MINIMUM_LIQUIDITY floor) ============

/// Burn `amount` LP straight through the token program — the path that
/// bypasses `RemoveLiquidity` (and its supply floor) entirely.
fn ix_direct_burn(env: &TestEnv, owner: &Pubkey, ata_lp: &Pubkey, amount: u64) -> Instruction {
    spl_token_2022::instruction::burn(
        &env.token_program,
        ata_lp,
        &env.lp_mint_pda().0,
        owner,
        &[],
        amount,
    )
    .unwrap()
}

#[tokio::test]
async fn remove_liquidity_below_minimum_liquidity_rejected() {
    let mut env = TestEnv::new().await;
    let (user, ata_a, ata_b, ata_lp) =
        env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;
    let supply = env.mint_supply(&env.lp_mint_pda().0).await;
    assert_eq!(supply, 200_000_000);

    // Leaving 1 .. MINIMUM_LIQUIDITY-1 units is rejected.
    for leave in [1, MINIMUM_LIQUIDITY - 1] {
        let ix = env.ix_remove_liquidity(
            &user.pubkey(),
            &ata_a,
            &ata_b,
            &ata_lp,
            supply - leave,
            1,
            1,
        );
        let err = env
            .send_with_new_blockhash(&[ix], &[&user])
            .await
            .unwrap_err();
        assert_eq!(
            extract_custom_error(&err),
            Some(err_code(LiquidityError::MinimumLiquidityFloor)),
            "leaving {leave} LP units must be rejected"
        );
    }
    assert_eq!(env.mint_supply(&env.lp_mint_pda().0).await, supply);

    // Landing exactly on the floor is allowed...
    let ix = env.ix_remove_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        supply - MINIMUM_LIQUIDITY,
        1,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&user]).await.unwrap();
    assert_eq!(env.mint_supply(&env.lp_mint_pda().0).await, MINIMUM_LIQUIDITY);

    // ...but nothing more except a full exit.
    let ix = env.ix_remove_liquidity(&user.pubkey(), &ata_a, &ata_b, &ata_lp, 1, 0, 0);
    let err = env
        .send_with_new_blockhash(&[ix], &[&user])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::MinimumLiquidityFloor))
    );

    // Full exit to exactly zero always works for a sole LP.
    let ix = env.ix_remove_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        MINIMUM_LIQUIDITY,
        1,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&user]).await.unwrap();
    assert_eq!(env.mint_supply(&env.lp_mint_pda().0).await, 0);
    assert_eq!(env.token_balance(&env.vault_a_pda().0).await, 0);
    assert_eq!(env.token_balance(&env.vault_b_pda().0).await, 0);
    assert_eq!(env.token_balance(&ata_a).await, 200_000_000);
    assert_eq!(env.token_balance(&ata_b).await, 800_000_000);

    // The emptied pool restarts with a normal first deposit.
    let ix = env.ix_add_liquidity(
        &user.pubkey(),
        &ata_a,
        &ata_b,
        &ata_lp,
        1_000_000,
        1_000_000,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&user]).await.unwrap();
    assert_eq!(env.mint_supply(&env.lp_mint_pda().0).await, 1_000_000);
}

/// The M21 adversarial scenario (tests/typescript/test_adversarial.ts,
/// "Inflation via remove-to-dust + donation"): seed with exactly
/// MIN_FIRST_DEPOSIT, RemoveLiquidity down to one LP unit, donate, then rob
/// the next depositor via LP floor-rounding. The 1-unit state is no longer
/// reachable through RemoveLiquidity, and even at the floor a donation cannot
/// extract value from a depositor.
#[tokio::test]
async fn remove_to_dust_plus_donation_attack_blocked() {
    let mut env = TestEnv::new().await;
    env.initialize_pool_default().await;
    let (attacker, att_a, att_b, att_lp) =
        env.setup_user(10_000_000_000, 1_000_000, 1_000_000).await;
    let ix = env.ix_add_liquidity(
        &attacker.pubkey(),
        &att_a,
        &att_b,
        &att_lp,
        1_000_000,
        1_000_000,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&attacker]).await.unwrap();
    assert_eq!(env.mint_supply(&env.lp_mint_pda().0).await, 1_000_000);

    // Step 1 of the attack — drain to a single LP unit — is rejected.
    let ix = env.ix_remove_liquidity(
        &attacker.pubkey(),
        &att_a,
        &att_b,
        &att_lp,
        999_999,
        1,
        1,
    );
    let err = env
        .send_with_new_blockhash(&[ix], &[&attacker])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::MinimumLiquidityFloor))
    );

    // Best the attacker can do: stop at the floor, then donate.
    let ix = env.ix_remove_liquidity(
        &attacker.pubkey(),
        &att_a,
        &att_b,
        &att_lp,
        1_000_000 - MINIMUM_LIQUIDITY,
        1,
        1,
    );
    env.send_with_new_blockhash(&[ix], &[&attacker]).await.unwrap();
    assert_eq!(env.mint_supply(&env.lp_mint_pda().0).await, MINIMUM_LIQUIDITY);
    let donation = 100_000_000;
    let vault_a = env.vault_a_pda().0;
    let vault_b = env.vault_b_pda().0;
    let mint_a = env.mint_a.pubkey();
    let mint_b = env.mint_b.pubkey();
    env.mint_to(&mint_a, &vault_a, donation).await;
    env.mint_to(&mint_b, &vault_b, donation).await;

    // Victim deposits 1.99× the donation per side.
    let deposit = 199_000_000;
    let (victim, v_a, v_b, v_lp) = env.setup_user(10_000_000_000, deposit, deposit).await;
    let ix = env.ix_add_liquidity(&victim.pubkey(), &v_a, &v_b, &v_lp, deposit, deposit, 1);
    env.send_with_new_blockhash(&[ix], &[&victim]).await.unwrap();

    let victim_lp = env.token_balance(&v_lp).await as u128;
    let paid_a = (deposit - env.token_balance(&v_a).await) as u128;
    let paid_b = (deposit - env.token_balance(&v_b).await) as u128;
    let supply = env.mint_supply(&env.lp_mint_pda().0).await as u128;
    let claim_a = env.token_balance(&vault_a).await as u128 * victim_lp / supply;
    let claim_b = env.token_balance(&vault_b).await as u128 * victim_lp / supply;
    // The victim's claim equals what they paid, to within 1 base unit per side.
    assert!(victim_lp > 0);
    assert!(paid_a - claim_a <= 1, "paid_a={paid_a} claim_a={claim_a}");
    assert!(paid_b - claim_b <= 1, "paid_b={paid_b} claim_b={claim_b}");
}

/// The floor in RemoveLiquidity can be bypassed by burning LP directly through
/// the token program. Even at supply == 1 with a large donation, AddLiquidity
/// charges a depositor only for the LP it mints, so nothing is stolen.
#[tokio::test]
async fn direct_burn_to_one_unit_cannot_steal_deposit() {
    let mut env = TestEnv::new().await;
    let (attacker, att_a, att_b, att_lp) =
        env.setup_pool_with_liquidity(1_000_000, 1_000_000).await;
    let ix = ix_direct_burn(&env, &attacker.pubkey(), &att_lp, 999_999);
    env.send_with_new_blockhash(&[ix], &[&attacker]).await.unwrap();
    assert_eq!(env.mint_supply(&env.lp_mint_pda().0).await, 1);

    let donation = 99_000_000;
    let vault_a = env.vault_a_pda().0;
    let vault_b = env.vault_b_pda().0;
    let mint_a = env.mint_a.pubkey();
    let mint_b = env.mint_b.pubkey();
    env.mint_to(&mint_a, &vault_a, donation).await;
    env.mint_to(&mint_b, &vault_b, donation).await;
    // Pool: 100M / 100M backing a single LP unit.

    // Victim offers 199M per side. Pre-fix: 1 LP for all 199M (= half of a
    // 299M pool), losing ~49.5M per side. Now: charged exactly 100M per side.
    let deposit = 199_000_000;
    let (victim, v_a, v_b, v_lp) = env.setup_user(10_000_000_000, deposit, deposit).await;
    let ix = env.ix_add_liquidity(&victim.pubkey(), &v_a, &v_b, &v_lp, deposit, deposit, 1);
    env.send_with_new_blockhash(&[ix], &[&victim]).await.unwrap();
    assert_eq!(env.token_balance(&v_lp).await, 1);
    assert_eq!(env.token_balance(&v_a).await, deposit - 100_000_000);
    assert_eq!(env.token_balance(&v_b).await, deposit - 100_000_000);

    // A deposit worth less than one LP unit reverts rather than being kept.
    let (small, s_a, s_b, s_lp) = env.setup_user(10_000_000_000, 50_000_000, 50_000_000).await;
    let ix = env.ix_add_liquidity(&small.pubkey(), &s_a, &s_b, &s_lp, 50_000_000, 50_000_000, 1);
    let err = env
        .send_with_new_blockhash(&[ix], &[&small])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::ZeroAmount))
    );
    assert_eq!(env.token_balance(&s_a).await, 50_000_000);

    // Pools already below the floor do not trap their LPs: the victim exits
    // (supply 2 → 1) and gets back exactly what they paid.
    let ix = env.ix_remove_liquidity(&victim.pubkey(), &v_a, &v_b, &v_lp, 1, 1, 1);
    env.send_with_new_blockhash(&[ix], &[&victim]).await.unwrap();
    assert_eq!(env.token_balance(&v_a).await, deposit);
    assert_eq!(env.token_balance(&v_b).await, deposit);

    // And the attacker can unwind to zero.
    let ix = env.ix_remove_liquidity(&attacker.pubkey(), &att_a, &att_b, &att_lp, 1, 1, 1);
    env.send_with_new_blockhash(&[ix], &[&attacker]).await.unwrap();
    assert_eq!(env.mint_supply(&env.lp_mint_pda().0).await, 0);
}

#[tokio::test]
async fn remove_liquidity_zero_output_rejected() {
    let mut env = TestEnv::new().await;
    // 100M A / 400M B, supply 200M: 1 LP is worth 0.5 A → rounds to 0.
    let (user, ata_a, ata_b, ata_lp) =
        env.setup_pool_with_liquidity(100_000_000, 400_000_000).await;
    let ix = env.ix_remove_liquidity(&user.pubkey(), &ata_a, &ata_b, &ata_lp, 1, 0, 0);
    let err = env
        .send_with_new_blockhash(&[ix], &[&user])
        .await
        .unwrap_err();
    assert_eq!(
        extract_custom_error(&err),
        Some(err_code(LiquidityError::ZeroAmount))
    );
    assert_eq!(env.token_balance(&ata_lp).await, 200_000_000);

    // 2 LP → exactly 1 A + 4 B is fine.
    let ix = env.ix_remove_liquidity(&user.pubkey(), &ata_a, &ata_b, &ata_lp, 2, 1, 4);
    env.send_with_new_blockhash(&[ix], &[&user]).await.unwrap();
    assert_eq!(env.token_balance(&ata_lp).await, 200_000_000 - 2);
}
