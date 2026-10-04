//! Burn LP tokens and withdraw proportional shares of accounted reserves.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    pubkey::Pubkey,
    sysvar::Sysvar,
};
use spl_token_2022::{
    extension::StateWithExtensions,
    state::{Account as TokenAccount, Mint},
};

use crate::{
    error::LiquidityError,
    events::{Event, LiquidityRemoved},
    instructions::add_liquidity::MINIMUM_LIQUIDITY,
    math::mul_div,
    state::{validate_token_program_for_mint, Pool, POOL_SEED},
};

pub fn process_remove_liquidity(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    lp_amount: u64,
    min_a_out: u64,
    min_b_out: u64,
) -> ProgramResult {
    let it = &mut accounts.iter();

    let pool_info = next_account_info(it)?;
    let vault_a_info = next_account_info(it)?;
    let vault_b_info = next_account_info(it)?;
    let lp_mint_info = next_account_info(it)?;
    let user_a_info = next_account_info(it)?;
    let user_b_info = next_account_info(it)?;
    let user_lp_info = next_account_info(it)?;
    let user_info = next_account_info(it)?;
    let mint_a_info = next_account_info(it)?;
    let mint_b_info = next_account_info(it)?;
    let token_program_a_info = next_account_info(it)?;
    let token_program_b_info = next_account_info(it)?;

    if !user_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    // Per-side token programs (LP mint rides on mint A's program).
    validate_token_program_for_mint(token_program_a_info, mint_a_info)?;
    validate_token_program_for_mint(token_program_b_info, mint_b_info)?;
    if pool_info.owner != program_id {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }
    if lp_amount == 0 {
        return Err(LiquidityError::ZeroAmount.into());
    }

    let mut pool = {
        let data = pool_info.try_borrow_data()?;
        Pool::try_from_slice(&data).map_err(|_| LiquidityError::AccountDataTooSmall)?
    };
    if !pool.is_initialized() {
        return Err(LiquidityError::NotInitialized.into());
    }
    if pool.vault_a != *vault_a_info.key
        || pool.vault_b != *vault_b_info.key
        || pool.lp_mint != *lp_mint_info.key
        || pool.mint_a != *mint_a_info.key
        || pool.mint_b != *mint_b_info.key
    {
        return Err(LiquidityError::InvalidPool.into());
    }

    let mint_a_decimals = read_mint_decimals(mint_a_info)?;
    let mint_b_decimals = read_mint_decimals(mint_b_info)?;
    let lp_decimals = read_mint_decimals(lp_mint_info)?;
    let lp_supply = read_mint_supply(lp_mint_info)?;
    let real_a = read_token_amount(vault_a_info)?;
    let real_b = read_token_amount(vault_b_info)?;

    if lp_supply == 0 {
        return Err(LiquidityError::ZeroReserves.into());
    }
    if (lp_amount as u128) > lp_supply {
        return Err(LiquidityError::MathUnderflow.into());
    }
    check_supply_floor(lp_supply, lp_amount as u128)?;

    // Capitalize accrued interest into the indexes before computing the
    // LP's proportional share — withdrawals shrink the pool's accounted
    // reserves, which would change the next instruction's utilization.
    pool.bump_indexes(real_a, real_b, Clock::get()?.slot)?;

    let (accounted_a, accounted_b) = pool.accounted(real_a, real_b)?;
    let (swappable_a, swappable_b) = pool.swappable(real_a, real_b)?;

    let (amount_a_out, amount_b_out) =
        withdrawal_amounts(lp_amount as u128, accounted_a, accounted_b, lp_supply)?;

    let amount_a_out_u64: u64 = amount_a_out
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;
    let amount_b_out_u64: u64 = amount_b_out
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;

    if amount_a_out_u64 < min_a_out || amount_b_out_u64 < min_b_out {
        return Err(LiquidityError::SlippageExceeded.into());
    }

    // Executable-reserve coverage check: pool may be heavily lent out and
    // unable to satisfy the proportional accounted withdrawal. Collateral
    // sitting in the vault is earmarked and not redeemable. Revert and let
    // the user wait for repayments / liquidations.
    if (amount_a_out_u64 as u128) > swappable_a || (amount_b_out_u64 as u128) > swappable_b {
        return Err(LiquidityError::InsufficientExecutableLiquidity.into());
    }

    // ---- Burn LP from user ----
    invoke(
        &spl_token_2022::instruction::burn_checked(
            token_program_a_info.key,
            user_lp_info.key,
            lp_mint_info.key,
            user_info.key,
            &[],
            lp_amount,
            lp_decimals,
        )?,
        &[
            user_lp_info.clone(),
            lp_mint_info.clone(),
            user_info.clone(),
        ],
    )?;

    // ---- Transfer A from vault → user (pool PDA signs) ----
    let pool_seeds: &[&[u8]] = &[
        POOL_SEED,
        pool.mint_a.as_ref(),
        pool.mint_b.as_ref(),
        std::slice::from_ref(&pool.pool_bump),
    ];
    invoke_signed(
        &spl_token_2022::instruction::transfer_checked(
            token_program_a_info.key,
            vault_a_info.key,
            mint_a_info.key,
            user_a_info.key,
            pool_info.key,
            &[],
            amount_a_out_u64,
            mint_a_decimals,
        )?,
        &[
            vault_a_info.clone(),
            mint_a_info.clone(),
            user_a_info.clone(),
            pool_info.clone(),
        ],
        &[pool_seeds],
    )?;

    // ---- Transfer B from vault → user ----
    invoke_signed(
        &spl_token_2022::instruction::transfer_checked(
            token_program_b_info.key,
            vault_b_info.key,
            mint_b_info.key,
            user_b_info.key,
            pool_info.key,
            &[],
            amount_b_out_u64,
            mint_b_decimals,
        )?,
        &[
            vault_b_info.clone(),
            mint_b_info.clone(),
            user_b_info.clone(),
            pool_info.clone(),
        ],
        &[pool_seeds],
    )?;

    pool.last_update_slot = Clock::get()?.slot;
    let mut data = pool_info.try_borrow_mut_data()?;
    pool.serialize(&mut &mut data[..])?;

    msg!(
        "RemoveLiquidity lp_in={} a_out={} b_out={}",
        lp_amount,
        amount_a_out_u64,
        amount_b_out_u64
    );
    LiquidityRemoved {
        pool: *pool_info.key,
        user: *user_info.key,
        lp_burned: lp_amount,
        amount_a_out: amount_a_out_u64,
        amount_b_out: amount_b_out_u64,
    }
    .emit();
    Ok(())
}

/// Enforce the LP-supply floor (`MINIMUM_LIQUIDITY`): a burn may not take a
/// pool from `supply >= MINIMUM_LIQUIDITY` to `0 < supply < MINIMUM_LIQUIDITY`.
///
/// - Exiting to exactly 0 is allowed, so a sole LP can always withdraw
///   everything. That does not reopen share inflation: with supply 0 the next
///   deposit is a first deposit minting `sqrt(a·b) >= MIN_FIRST_DEPOSIT` LP
///   regardless of vault balances, so anything left in (or donated to) the
///   vaults is gifted to that depositor — only the donor loses. Nothing else
///   can be outstanding at supply 0 when it is reached through this
///   instruction: a full exit withdraws `accounted = swappable + total_debt`
///   and must pass the `<= swappable` coverage check, so it only succeeds
///   with `total_debt_a == total_debt_b == 0`. Every open loan carries a
///   nonzero principal (OpenLoan rejects zero debt; RepayLoan closes the loan
///   in full), so zero total debt means zero open loans and therefore zero
///   earmarked collateral — and collateral is excluded from `accounted`
///   anyway, so a first depositor can neither claim it nor be diluted by it.
///   (Supply 0 with loans still open is only reachable by holders burning
///   LP directly through the token program; the outstanding debt claim is
///   then gifted to the next first depositor, the collateral stays
///   earmarked for its borrower — again only the burner loses.)
/// - Pools already below the floor (created before it existed, or shrunk by
///   holders burning LP directly through the token program, which no program
///   check can prevent) are not frozen: burns from there are allowed so their
///   LPs are never trapped. Deposits into such pools are still safe because
///   `AddLiquidity` charges only for the LP actually minted.
/// - Trade-off (chosen over Uniswap-v2's permanent burn of the first 1000
///   units, which would break "a sole LP can always fully exit"): the floor
///   is on total supply, so if the other holders own fewer than
///   `MINIMUM_LIQUIDITY` units between them, an LP can only withdraw down to
///   the floor and must leave `< MINIMUM_LIQUIDITY` units until they exit.
fn check_supply_floor(lp_supply: u128, lp_amount: u128) -> Result<(), LiquidityError> {
    let post_supply = lp_supply
        .checked_sub(lp_amount)
        .ok_or(LiquidityError::MathUnderflow)?;
    let floor = MINIMUM_LIQUIDITY as u128;
    if lp_supply >= floor && post_supply != 0 && post_supply < floor {
        return Err(LiquidityError::MinimumLiquidityFloor);
    }
    Ok(())
}

/// Proportional share `floor(lp · accounted / supply)` of each side.
///
/// Rejects a burn whose output on a side rounds to 0 while that side has a
/// nonzero accounted reserve: the LP would be burned for less than it is
/// owed on that side. A side whose accounted reserve is genuinely 0 (e.g. all
/// of it was lent out and then forgiven by a liquidation) legitimately pays
/// 0 — rejecting that would trap every LP of such a pool, so it is allowed.
/// The residual cost is that a holder of fewer than `supply / accounted_x`
/// LP units cannot withdraw on their own; that position is worth less than
/// one base unit of side x (and the pool-price equivalent on the other
/// side), and can still be merged with more LP before burning.
fn withdrawal_amounts(
    lp_amount: u128,
    accounted_a: u128,
    accounted_b: u128,
    lp_supply: u128,
) -> Result<(u128, u128), LiquidityError> {
    let amount_a_out = mul_div(lp_amount, accounted_a, lp_supply)?;
    let amount_b_out = mul_div(lp_amount, accounted_b, lp_supply)?;
    if (amount_a_out == 0 && accounted_a != 0) || (amount_b_out == 0 && accounted_b != 0) {
        return Err(LiquidityError::ZeroAmount);
    }
    Ok((amount_a_out, amount_b_out))
}

fn read_mint_decimals(info: &AccountInfo) -> Result<u8, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state =
        StateWithExtensions::<Mint>::unpack(&data).map_err(|_| LiquidityError::InvalidPoolMint)?;
    Ok(state.base.decimals)
}

fn read_mint_supply(info: &AccountInfo) -> Result<u128, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state =
        StateWithExtensions::<Mint>::unpack(&data).map_err(|_| LiquidityError::InvalidPoolMint)?;
    Ok(state.base.supply as u128)
}

fn read_token_amount(info: &AccountInfo) -> Result<u128, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&data)
        .map_err(|_| LiquidityError::InvalidVault)?;
    Ok(state.base.amount as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u128 = MINIMUM_LIQUIDITY as u128;

    #[test]
    fn floor_rejects_dust_supply() {
        let supply = 1_000_000;
        assert_eq!(
            check_supply_floor(supply, supply - 1),
            Err(LiquidityError::MinimumLiquidityFloor)
        );
        assert_eq!(
            check_supply_floor(supply, supply - (M - 1)),
            Err(LiquidityError::MinimumLiquidityFloor)
        );
        // Landing exactly on the floor is fine; so is a full exit.
        assert_eq!(check_supply_floor(supply, supply - M), Ok(()));
        assert_eq!(check_supply_floor(supply, supply), Ok(()));
        assert_eq!(check_supply_floor(M, M), Ok(()));
        assert_eq!(
            check_supply_floor(M, 1),
            Err(LiquidityError::MinimumLiquidityFloor)
        );
    }

    #[test]
    fn floor_does_not_trap_pools_already_below_it() {
        // Legacy pool / directly-burned supply: LPs can still leave.
        assert_eq!(check_supply_floor(M - 1, 1), Ok(()));
        assert_eq!(check_supply_floor(500, 250), Ok(()));
        assert_eq!(check_supply_floor(1, 1), Ok(()));
    }

    #[test]
    fn zero_output_rejected_when_side_nonempty() {
        // 100M A / 400M B, supply 200M: 1 LP → 0.5 A (rounds to 0).
        assert_eq!(
            withdrawal_amounts(1, 100_000_000, 400_000_000, 200_000_000),
            Err(LiquidityError::ZeroAmount)
        );
        assert_eq!(
            withdrawal_amounts(2, 100_000_000, 400_000_000, 200_000_000),
            Ok((1, 4))
        );
    }

    #[test]
    fn genuinely_empty_side_pays_zero() {
        // accounted_a == 0 (e.g. fully lent out, then forgiven by a
        // liquidation): LPs must still be able to withdraw the other side.
        assert_eq!(withdrawal_amounts(10, 0, 1_000, 100), Ok((0, 100)));
        assert_eq!(withdrawal_amounts(100, 0, 1_000, 100), Ok((0, 1_000)));
    }
}
