//! Deposit liquidity and mint LP tokens against accounted reserves.

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
    events::{Event, LiquidityAdded},
    math::{isqrt_u128, mul_div},
    state::{is_valid_token_program, Pool, POOL_SEED},
};

/// Minimum first-deposit per side. Prevents share-inflation tricks where the
/// initial depositor seeds the pool with dust to make subsequent deposits
/// round to zero LP.
pub const MIN_FIRST_DEPOSIT: u64 = 1_000_000;

pub fn process_add_liquidity(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount_a_max: u64,
    amount_b_max: u64,
    min_lp_out: u64,
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
    let token_program_info = next_account_info(it)?;

    // ---- Validation ----
    if !user_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    if !is_valid_token_program(token_program_info.key) {
        return Err(LiquidityError::InvalidTokenProgram.into());
    }
    if pool_info.owner != program_id {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }
    if amount_a_max == 0 || amount_b_max == 0 {
        return Err(LiquidityError::ZeroAmount.into());
    }

    // Load pool
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

    // Bump indexes before we read accounted reserves: deposits change
    // utilization, so we want the elapsed period accrued at the previous
    // utilization first.
    pool.bump_indexes(real_a, real_b, Clock::get()?.slot)?;

    // accounted_x = (real_x - total_collateral_x) + total_debt_x —
    // collateral is in the vault but earmarked, not part of LP claim.
    let (accounted_a, accounted_b) = pool.accounted(real_a, real_b)?;

    // ---- Deposit-amount derivation ----
    let (amount_a_in, amount_b_in, lp_to_mint) = if lp_supply == 0 {
        // First deposit: take both maxes; LP = sqrt(a * b).
        if (amount_a_max as u128) < MIN_FIRST_DEPOSIT as u128
            || (amount_b_max as u128) < MIN_FIRST_DEPOSIT as u128
        {
            return Err(LiquidityError::ZeroAmount.into());
        }
        let lp = isqrt_u128((amount_a_max as u128) * (amount_b_max as u128));
        if lp == 0 {
            return Err(LiquidityError::ZeroAmount.into());
        }
        (amount_a_max as u128, amount_b_max as u128, lp)
    } else {
        if accounted_a == 0 || accounted_b == 0 {
            return Err(LiquidityError::ZeroReserves.into());
        }
        // ideal_b matched against amount_a_max
        let ideal_b = mul_div(amount_a_max as u128, accounted_b, accounted_a)?;
        let (a_in, b_in) = if ideal_b <= amount_b_max as u128 {
            (amount_a_max as u128, ideal_b)
        } else {
            let ideal_a = mul_div(amount_b_max as u128, accounted_a, accounted_b)?;
            (ideal_a, amount_b_max as u128)
        };
        if a_in == 0 || b_in == 0 {
            return Err(LiquidityError::ZeroAmount.into());
        }
        // LP = min(a/A, b/B) * lp_supply
        let lp_a = mul_div(a_in, lp_supply, accounted_a)?;
        let lp_b = mul_div(b_in, lp_supply, accounted_b)?;
        let lp = lp_a.min(lp_b);
        if lp == 0 {
            return Err(LiquidityError::ZeroAmount.into());
        }
        (a_in, b_in, lp)
    };

    // u64 bound checks
    let amount_a_in_u64: u64 = amount_a_in
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;
    let amount_b_in_u64: u64 = amount_b_in
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;
    let lp_to_mint_u64: u64 = lp_to_mint
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;

    if lp_to_mint_u64 < min_lp_out {
        return Err(LiquidityError::SlippageExceeded.into());
    }

    // ---- Transfer A from user → vault A ----
    invoke(
        &spl_token_2022::instruction::transfer_checked(
            token_program_info.key,
            user_a_info.key,
            mint_a_info.key,
            vault_a_info.key,
            user_info.key,
            &[],
            amount_a_in_u64,
            mint_a_decimals,
        )?,
        &[
            user_a_info.clone(),
            mint_a_info.clone(),
            vault_a_info.clone(),
            user_info.clone(),
        ],
    )?;

    // ---- Transfer B from user → vault B ----
    invoke(
        &spl_token_2022::instruction::transfer_checked(
            token_program_info.key,
            user_b_info.key,
            mint_b_info.key,
            vault_b_info.key,
            user_info.key,
            &[],
            amount_b_in_u64,
            mint_b_decimals,
        )?,
        &[
            user_b_info.clone(),
            mint_b_info.clone(),
            vault_b_info.clone(),
            user_info.clone(),
        ],
    )?;

    // ---- Mint LP to user (mint authority = pool PDA) ----
    let pool_seeds: &[&[u8]] = &[
        POOL_SEED,
        pool.mint_a.as_ref(),
        pool.mint_b.as_ref(),
        std::slice::from_ref(&pool.pool_bump),
    ];
    invoke_signed(
        &spl_token_2022::instruction::mint_to_checked(
            token_program_info.key,
            lp_mint_info.key,
            user_lp_info.key,
            pool_info.key,
            &[],
            lp_to_mint_u64,
            lp_decimals,
        )?,
        &[
            lp_mint_info.clone(),
            user_lp_info.clone(),
            pool_info.clone(),
        ],
        &[pool_seeds],
    )?;

    // No pool fields change on add_liquidity (LP supply lives on the mint, debt
    // totals are unaffected, real reserves live in the vaults). Touch
    // last_update_slot so off-chain indexers see activity.
    pool.last_update_slot = Clock::get()?.slot;
    let mut data = pool_info.try_borrow_mut_data()?;
    pool.serialize(&mut &mut data[..])?;

    msg!(
        "AddLiquidity a_in={} b_in={} lp_out={}",
        amount_a_in_u64,
        amount_b_in_u64,
        lp_to_mint_u64
    );
    LiquidityAdded {
        pool: *pool_info.key,
        user: *user_info.key,
        amount_a_in: amount_a_in_u64,
        amount_b_in: amount_b_in_u64,
        lp_minted: lp_to_mint_u64,
    }
    .emit();
    Ok(())
}

fn read_mint_decimals(info: &AccountInfo) -> Result<u8, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state = StateWithExtensions::<Mint>::unpack(&data)
        .map_err(|_| LiquidityError::InvalidPoolMint)?;
    Ok(state.base.decimals)
}

fn read_mint_supply(info: &AccountInfo) -> Result<u128, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state = StateWithExtensions::<Mint>::unpack(&data)
        .map_err(|_| LiquidityError::InvalidPoolMint)?;
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
