//! Repay a loan in full and release collateral.
//!
//! Computes interest accrued via the borrow index, transfers `principal +
//! accrued` of the debt token from the borrower into the vault, transfers
//! `collateral_amount` of the collateral token from the vault back to the
//! borrower, decrements the loan's band membership `count` (clearing the Pool
//! band bitmap bit and refunding the band's rent if it became empty), marks the
//! `Loan` as repaid, and refunds the `Loan` rent to the borrower.

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
    events::{Event, LoanRepaid},
    math::LoanSides,
    state::{
        bitmap_clear, validate_token_program_for_mint, Loan, LoanIndexBand, Pool, POOL_SEED,
    },
};

pub fn process_repay_loan(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let it = &mut accounts.iter();

    let pool_info = next_account_info(it)?;
    let vault_a_info = next_account_info(it)?;
    let vault_b_info = next_account_info(it)?;
    let user_a_info = next_account_info(it)?;
    let user_b_info = next_account_info(it)?;
    let mint_a_info = next_account_info(it)?;
    let mint_b_info = next_account_info(it)?;
    let borrower_info = next_account_info(it)?;
    let loan_info = next_account_info(it)?;
    let band_info = next_account_info(it)?;
    let token_program_a_info = next_account_info(it)?;
    let token_program_b_info = next_account_info(it)?;

    if !borrower_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    validate_token_program_for_mint(token_program_a_info, mint_a_info)?;
    validate_token_program_for_mint(token_program_b_info, mint_b_info)?;
    if pool_info.owner != program_id
        || loan_info.owner != program_id
        || band_info.owner != program_id
    {
        return Err(LiquidityError::InvalidAccountOwner.into());
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
        || pool.mint_a != *mint_a_info.key
        || pool.mint_b != *mint_b_info.key
    {
        return Err(LiquidityError::InvalidPool.into());
    }

    let mut loan = {
        let data = loan_info.try_borrow_data()?;
        Loan::try_from_slice(&data).map_err(|_| LiquidityError::AccountDataTooSmall)?
    };
    if !loan.is_initialized() {
        return Err(LiquidityError::NotInitialized.into());
    }
    if !loan.is_open() {
        return Err(LiquidityError::LoanNotOpen.into());
    }
    if loan.pool != *pool_info.key || loan.borrower != *borrower_info.key {
        return Err(LiquidityError::InvalidPool.into());
    }

    // The loan's cached band_id + trigger_direction identify its band PDA.
    let mut band = {
        let data = band_info.try_borrow_data()?;
        LoanIndexBand::try_from_slice(&data)
            .map_err(|_| LiquidityError::AccountDataTooSmall)?
    };
    if !band.is_initialized()
        || band.pool != *pool_info.key
        || band.band_id != loan.band_id
        || band.direction != loan.trigger_direction
    {
        return Err(LiquidityError::BandMismatch.into());
    }
    let (expected_band, _) = LoanIndexBand::derive_pda(
        pool_info.key,
        loan.trigger_direction,
        loan.band_id,
        program_id,
    );
    if expected_band != *band_info.key {
        return Err(LiquidityError::InvalidPDA.into());
    }

    let clock = Clock::get()?;

    // ---- Bump indexes; compute owed via index ratio ----
    let real_a_pre = read_token_amount(vault_a_info)?;
    let real_b_pre = read_token_amount(vault_b_info)?;
    pool.bump_indexes(real_a_pre, real_b_pre, clock.slot)?;
    let cur_index = pool.borrow_index_for_debt_side(loan.sides)?;
    let total_owed = loan.owed(cur_index)?;
    loan.last_touch_slot = clock.slot;
    let total_owed_u64: u64 = total_owed
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;

    let sides = LoanSides::from_u8(loan.sides)?;
    let collateral_amount_u64: u64 = loan
        .collateral_amount
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;

    // ---- Token transfers ----
    let mint_a_decimals = read_mint_decimals(mint_a_info)?;
    let mint_b_decimals = read_mint_decimals(mint_b_info)?;
    let (
        debt_user_info,
        debt_vault_info,
        debt_mint_info,
        debt_decimals,
        debt_token_program_info,
        collateral_user_info,
        collateral_vault_info,
        collateral_mint_info,
        collateral_decimals,
        collateral_token_program_info,
    ) = match sides {
        LoanSides::CollateralA => (
            user_b_info,
            vault_b_info,
            mint_b_info,
            mint_b_decimals,
            token_program_b_info,
            user_a_info,
            vault_a_info,
            mint_a_info,
            mint_a_decimals,
            token_program_a_info,
        ),
        LoanSides::CollateralB => (
            user_a_info,
            vault_a_info,
            mint_a_info,
            mint_a_decimals,
            token_program_a_info,
            user_b_info,
            vault_b_info,
            mint_b_info,
            mint_b_decimals,
            token_program_b_info,
        ),
    };

    // Borrower → vault: principal + accrued
    invoke(
        &spl_token_2022::instruction::transfer_checked(
            debt_token_program_info.key,
            debt_user_info.key,
            debt_mint_info.key,
            debt_vault_info.key,
            borrower_info.key,
            &[],
            total_owed_u64,
            debt_decimals,
        )?,
        &[
            debt_user_info.clone(),
            debt_mint_info.clone(),
            debt_vault_info.clone(),
            borrower_info.clone(),
        ],
    )?;

    // Vault → borrower: collateral
    let pool_pda_seeds: &[&[u8]] = &[
        POOL_SEED,
        pool.mint_a.as_ref(),
        pool.mint_b.as_ref(),
        std::slice::from_ref(&pool.pool_bump),
    ];
    invoke_signed(
        &spl_token_2022::instruction::transfer_checked(
            collateral_token_program_info.key,
            collateral_vault_info.key,
            collateral_mint_info.key,
            collateral_user_info.key,
            pool_info.key,
            &[],
            collateral_amount_u64,
            collateral_decimals,
        )?,
        &[
            collateral_vault_info.clone(),
            collateral_mint_info.clone(),
            collateral_user_info.clone(),
            pool_info.clone(),
        ],
        &[pool_pda_seeds],
    )?;

    // ---- Pool accounting: principal-only debt removal; accrued interest stays
    // in the vault as inventory (effectively LP yield). Collateral leaves the
    // collateral total. ----
    match sides {
        LoanSides::CollateralA => {
            pool.total_collateral_a = pool
                .total_collateral_a
                .checked_sub(loan.collateral_amount)
                .ok_or(LiquidityError::MathUnderflow)?;
            pool.total_debt_b = pool
                .total_debt_b
                .checked_sub(loan.debt_principal)
                .ok_or(LiquidityError::MathUnderflow)?;
        }
        LoanSides::CollateralB => {
            pool.total_collateral_b = pool
                .total_collateral_b
                .checked_sub(loan.collateral_amount)
                .ok_or(LiquidityError::MathUnderflow)?;
            pool.total_debt_a = pool
                .total_debt_a
                .checked_sub(loan.debt_principal)
                .ok_or(LiquidityError::MathUnderflow)?;
        }
    }
    pool.open_loans = pool
        .open_loans
        .checked_sub(1)
        .ok_or(LiquidityError::MathUnderflow)?;
    pool.last_update_slot = clock.slot;

    // ---- Drop this loan from its band's membership ----
    band.count = band
        .count
        .checked_sub(1)
        .ok_or(LiquidityError::MathUnderflow)?;

    let band_now_empty = band.count == 0;
    if !band_now_empty {
        let mut data = band_info.try_borrow_mut_data()?;
        band.serialize(&mut &mut data[..])?;
    } else {
        // Band is empty — clear its presence bit and refund its rent.
        bitmap_clear(pool.band_bitmap_mut(band.direction)?, band.band_id)?;
        close_account(band_info, borrower_info)?;
    }

    // Mark Loan as repaid and close (refund rent).
    loan.status = Loan::STATUS_REPAID;
    loan.closed_slot = clock.slot;
    {
        // Briefly persist the closed state so off-chain indexers see it before
        // close_account zeroes the data. This is mostly for symmetry — once
        // close_account runs, the data is gone.
        let mut data = loan_info.try_borrow_mut_data()?;
        loan.serialize(&mut &mut data[..])?;
    }
    close_account(loan_info, borrower_info)?;

    {
        let mut data = pool_info.try_borrow_mut_data()?;
        pool.serialize(&mut &mut data[..])?;
    }

    msg!(
        "RepayLoan principal={} owed={} band_now_empty={}",
        loan.debt_principal,
        total_owed_u64,
        band_now_empty
    );
    LoanRepaid {
        pool: *pool_info.key,
        loan: *loan_info.key,
        borrower: *borrower_info.key,
        debt_principal: loan.debt_principal,
        total_owed: total_owed_u64,
    }
    .emit();
    Ok(())
}

/// Drain lamports to `dest` and zero the data; with zero lamports the
/// runtime garbage-collects the account at the end of the transaction.
fn close_account<'a>(
    account: &AccountInfo<'a>,
    dest: &AccountInfo<'a>,
) -> Result<(), solana_program::program_error::ProgramError> {
    let lamports = account.lamports();
    **account.try_borrow_mut_lamports()? = 0;
    **dest.try_borrow_mut_lamports()? = dest
        .lamports()
        .checked_add(lamports)
        .ok_or(LiquidityError::MathOverflow)?;
    let mut data = account.try_borrow_mut_data()?;
    for byte in data.iter_mut() {
        *byte = 0;
    }
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

#[allow(dead_code)]
fn read_token_amount(info: &AccountInfo) -> Result<u128, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&data)
        .map_err(|_| LiquidityError::InvalidVault)?;
    Ok(state.base.amount as u128)
}
