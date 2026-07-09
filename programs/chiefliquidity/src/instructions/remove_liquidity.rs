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

    // Capitalize accrued interest into the indexes before computing the
    // LP's proportional share — withdrawals shrink the pool's accounted
    // reserves, which would change the next instruction's utilization.
    pool.bump_indexes(real_a, real_b, Clock::get()?.slot)?;

    let (accounted_a, accounted_b) = pool.accounted(real_a, real_b)?;
    let (swappable_a, swappable_b) = pool.swappable(real_a, real_b)?;

    let amount_a_out = mul_div(lp_amount as u128, accounted_a, lp_supply)?;
    let amount_b_out = mul_div(lp_amount as u128, accounted_b, lp_supply)?;

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
