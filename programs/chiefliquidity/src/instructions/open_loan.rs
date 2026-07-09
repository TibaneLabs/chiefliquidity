//! Open a collateralized loan against the pool.
//!
//! Creates a `Loan` and allocates the corresponding `LoanIndexBand` on first
//! use, incrementing its membership `count` (and setting the Pool band bitmap
//! when the band goes from empty to populated). Updates `pool.total_debt_x`,
//! `pool.total_collateral_y`, `pool.open_loans`, `pool.next_loan_nonce`.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};
use spl_token_2022::{
    extension::StateWithExtensions,
    state::{Account as TokenAccount, Mint},
};

use crate::{
    error::LiquidityError,
    events::{Event, LoanOpened},
    math::{band_id_for_trigger, mul_div, recompute_trigger, LoanSides, BPS_DENOM},
    state::{
        bitmap_set, validate_token_program_for_mint, Loan, LoanIndexBand, Pool, BAND_SEED,
        LOAN_DISCRIMINATOR, LOAN_INDEX_BAND_DISCRIMINATOR, LOAN_SEED, POOL_SEED,
    },
};

#[allow(clippy::too_many_arguments)]
pub fn process_open_loan(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    sides_byte: u8,
    collateral_amount: u64,
    debt_amount: u64,
    nonce: u64,
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
    let system_program_info = next_account_info(it)?;
    let token_program_a_info = next_account_info(it)?;
    let token_program_b_info = next_account_info(it)?;

    if !borrower_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    validate_token_program_for_mint(token_program_a_info, mint_a_info)?;
    validate_token_program_for_mint(token_program_b_info, mint_b_info)?;
    if pool_info.owner != program_id {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }
    if collateral_amount == 0 || debt_amount == 0 {
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
        || pool.mint_a != *mint_a_info.key
        || pool.mint_b != *mint_b_info.key
    {
        return Err(LiquidityError::InvalidPool.into());
    }
    if nonce != pool.next_loan_nonce {
        return Err(LiquidityError::InvalidInstruction.into());
    }

    let sides = LoanSides::from_u8(sides_byte)?;

    // Bump per-side borrow indexes BEFORE reading anything that depends on
    // utilization (so the new loan's snapshot is the freshly-bumped value).
    let clock_for_bump = Clock::get()?;
    let pre_real_a = read_token_amount(vault_a_info)?;
    let pre_real_b = read_token_amount(vault_b_info)?;
    pool.bump_indexes(pre_real_a, pre_real_b, clock_for_bump.slot)?;

    // ---- Compute trigger price + LTV ----
    let (trigger_price_wad, direction) = recompute_trigger(
        sides,
        collateral_amount as u128,
        debt_amount as u128,
        pool.liq_ratio_bps,
    )?;

    let real_a = read_token_amount(vault_a_info)?;
    let real_b = read_token_amount(vault_b_info)?;
    let (accounted_a, accounted_b) = pool.accounted(real_a, real_b)?;
    let (swappable_a, swappable_b) = pool.swappable(real_a, real_b)?;
    if accounted_a == 0 || accounted_b == 0 {
        return Err(LiquidityError::ZeroReserves.into());
    }

    // ltv = debt_value / collateral_value, both in the same token's units via
    // the pool mid-price.
    //   CollateralA, DebtB:   ltv = debt * accounted_a / (collateral * accounted_b)
    //   CollateralB, DebtA:   ltv = debt * accounted_b / (collateral * accounted_a)
    let ltv_bps = match sides {
        LoanSides::CollateralA => mul_div(
            (debt_amount as u128) * BPS_DENOM,
            accounted_a,
            (collateral_amount as u128) * accounted_b,
        )?,
        LoanSides::CollateralB => mul_div(
            (debt_amount as u128) * BPS_DENOM,
            accounted_b,
            (collateral_amount as u128) * accounted_a,
        )?,
    };
    if ltv_bps > pool.max_ltv_bps as u128 {
        msg!("ltv_bps {} > max_ltv_bps {}", ltv_bps, pool.max_ltv_bps);
        return Err(LiquidityError::LtvExceedsMax.into());
    }

    // Executable-reserve coverage: must be able to actually pay out the debt
    // from the LP-owned share (collateral in the vault is earmarked).
    let debt_swappable = match sides {
        LoanSides::CollateralA => swappable_b,
        LoanSides::CollateralB => swappable_a,
    };
    if (debt_amount as u128) > debt_swappable {
        return Err(LiquidityError::InsufficientExecutableLiquidity.into());
    }

    // ---- PDA derivations ----
    let (expected_loan, loan_bump) =
        Loan::derive_pda(pool_info.key, borrower_info.key, nonce, program_id);
    if *loan_info.key != expected_loan {
        return Err(LiquidityError::InvalidPDA.into());
    }
    let band_id = band_id_for_trigger(trigger_price_wad)?;
    let direction_byte = direction as u8;
    let (expected_band, band_bump) =
        LoanIndexBand::derive_pda(pool_info.key, direction_byte, band_id, program_id);
    if *band_info.key != expected_band {
        return Err(LiquidityError::BandMismatch.into());
    }
    if !loan_info.data_is_empty() {
        return Err(LiquidityError::AlreadyInitialized.into());
    }

    let rent = Rent::get()?;
    let clock = Clock::get()?;

    // ---- Allocate Loan ----
    let nonce_le = nonce.to_le_bytes();
    let loan_seeds: &[&[u8]] = &[
        LOAN_SEED,
        pool_info.key.as_ref(),
        borrower_info.key.as_ref(),
        &nonce_le,
        std::slice::from_ref(&loan_bump),
    ];
    invoke_signed(
        &system_instruction::create_account(
            borrower_info.key,
            loan_info.key,
            rent.minimum_balance(Loan::LEN),
            Loan::LEN as u64,
            program_id,
        ),
        &[
            borrower_info.clone(),
            loan_info.clone(),
            system_program_info.clone(),
        ],
        &[loan_seeds],
    )?;

    // ---- Allocate Band on first use ----
    let band_id_le = band_id.to_le_bytes();
    let band_seeds: &[&[u8]] = &[
        BAND_SEED,
        pool_info.key.as_ref(),
        std::slice::from_ref(&direction_byte),
        &band_id_le,
        std::slice::from_ref(&band_bump),
    ];
    if band_info.data_is_empty() {
        invoke_signed(
            &system_instruction::create_account(
                borrower_info.key,
                band_info.key,
                rent.minimum_balance(LoanIndexBand::LEN),
                LoanIndexBand::LEN as u64,
                program_id,
            ),
            &[
                borrower_info.clone(),
                band_info.clone(),
                system_program_info.clone(),
            ],
            &[band_seeds],
        )?;
        let new_band = LoanIndexBand {
            discriminator: LOAN_INDEX_BAND_DISCRIMINATOR,
            pool: *pool_info.key,
            band_id,
            direction: direction_byte,
            bump: band_bump,
            _pad: [0; 2],
            count: 0,
            _pad2: [0; 4],
            _reserved: [0; 32],
        };
        let mut data = band_info.try_borrow_mut_data()?;
        new_band.serialize(&mut &mut data[..])?;
    }

    // Load (and update) band
    let mut band = {
        let data = band_info.try_borrow_data()?;
        LoanIndexBand::try_from_slice(&data)
            .map_err(|_| LiquidityError::AccountDataTooSmall)?
    };
    if !band.is_initialized()
        || band.pool != *pool_info.key
        || band.band_id != band_id
        || band.direction != direction_byte
    {
        return Err(LiquidityError::BandMismatch.into());
    }
    if band.count >= LoanIndexBand::MAX_LOANS {
        return Err(LiquidityError::BandFull.into());
    }
    if band.count == 0 {
        // Band goes from empty → populated: set its presence bit. This must
        // key off `count`, not off PDA allocation — a swap that liquidates a
        // band's last loan clears the bit but leaves the PDA allocated, and a
        // loan added to such a band would otherwise be invisible to the swap
        // completeness proof (it could never be liquidated).
        bitmap_set(pool.band_bitmap_mut(direction_byte)?, band_id)?;
    }

    // ---- Persist Loan ----
    // Snapshot the borrow index for this loan's debt side as of right now
    // (after the bump above). Owed = principal initially.
    let borrow_index_snapshot_wad = pool.borrow_index_for_debt_side(sides_byte)?;
    let loan = Loan {
        discriminator: LOAN_DISCRIMINATOR,
        pool: *pool_info.key,
        borrower: *borrower_info.key,
        nonce,
        bump: loan_bump,
        sides: sides_byte,
        collateral_amount: collateral_amount as u128,
        debt_principal: debt_amount as u128,
        borrow_index_snapshot_wad,
        last_touch_slot: clock.slot,
        trigger_price_wad,
        trigger_direction: direction_byte,
        status: Loan::STATUS_OPEN,
        _status_pad: [0; 6],
        band_id,
        opened_slot: clock.slot,
        closed_slot: 0,
        _reserved: [0; 28],
    };
    {
        let mut data = loan_info.try_borrow_mut_data()?;
        loan.serialize(&mut &mut data[..])?;
    }

    // ---- Update Band: one more member ----
    band.count = band
        .count
        .checked_add(1)
        .ok_or(LiquidityError::MathOverflow)?;
    {
        let mut data = band_info.try_borrow_mut_data()?;
        band.serialize(&mut &mut data[..])?;
    }

    // ---- Token transfers ----
    let mint_a_decimals = read_mint_decimals(mint_a_info)?;
    let mint_b_decimals = read_mint_decimals(mint_b_info)?;

    let (
        collateral_user_info,
        collateral_vault_info,
        collateral_mint_info,
        collateral_decimals,
        collateral_token_program_info,
        debt_user_info,
        debt_vault_info,
        debt_mint_info,
        debt_decimals,
        debt_token_program_info,
    ) = match sides {
        LoanSides::CollateralA => (
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
        LoanSides::CollateralB => (
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
    };

    // Collateral: borrower → vault
    invoke(
        &spl_token_2022::instruction::transfer_checked(
            collateral_token_program_info.key,
            collateral_user_info.key,
            collateral_mint_info.key,
            collateral_vault_info.key,
            borrower_info.key,
            &[],
            collateral_amount,
            collateral_decimals,
        )?,
        &[
            collateral_user_info.clone(),
            collateral_mint_info.clone(),
            collateral_vault_info.clone(),
            borrower_info.clone(),
        ],
    )?;

    // Debt: vault → borrower (pool PDA signs)
    let pool_pda_seeds: &[&[u8]] = &[
        POOL_SEED,
        pool.mint_a.as_ref(),
        pool.mint_b.as_ref(),
        std::slice::from_ref(&pool.pool_bump),
    ];
    invoke_signed(
        &spl_token_2022::instruction::transfer_checked(
            debt_token_program_info.key,
            debt_vault_info.key,
            debt_mint_info.key,
            debt_user_info.key,
            pool_info.key,
            &[],
            debt_amount,
            debt_decimals,
        )?,
        &[
            debt_vault_info.clone(),
            debt_mint_info.clone(),
            debt_user_info.clone(),
            pool_info.clone(),
        ],
        &[pool_pda_seeds],
    )?;

    // ---- Update Pool ----
    match sides {
        LoanSides::CollateralA => {
            pool.total_collateral_a = pool
                .total_collateral_a
                .checked_add(collateral_amount as u128)
                .ok_or(LiquidityError::MathOverflow)?;
            pool.total_debt_b = pool
                .total_debt_b
                .checked_add(debt_amount as u128)
                .ok_or(LiquidityError::MathOverflow)?;
        }
        LoanSides::CollateralB => {
            pool.total_collateral_b = pool
                .total_collateral_b
                .checked_add(collateral_amount as u128)
                .ok_or(LiquidityError::MathOverflow)?;
            pool.total_debt_a = pool
                .total_debt_a
                .checked_add(debt_amount as u128)
                .ok_or(LiquidityError::MathOverflow)?;
        }
    }
    pool.open_loans = pool
        .open_loans
        .checked_add(1)
        .ok_or(LiquidityError::MathOverflow)?;
    pool.next_loan_nonce = pool
        .next_loan_nonce
        .checked_add(1)
        .ok_or(LiquidityError::MathOverflow)?;
    pool.last_update_slot = clock.slot;
    {
        let mut data = pool_info.try_borrow_mut_data()?;
        pool.serialize(&mut &mut data[..])?;
    }

    msg!(
        "OpenLoan nonce={} sides={} coll={} debt={} band={} dir={} trigger_wad={}",
        nonce,
        sides_byte,
        collateral_amount,
        debt_amount,
        band_id,
        direction_byte,
        trigger_price_wad
    );
    LoanOpened {
        pool: *pool_info.key,
        loan: *loan_info.key,
        borrower: *borrower_info.key,
        nonce,
        sides: sides_byte,
        collateral_amount,
        debt_amount,
        band_id,
        trigger_direction: direction_byte,
        trigger_price_wad,
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

fn read_token_amount(info: &AccountInfo) -> Result<u128, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&data)
        .map_err(|_| LiquidityError::InvalidVault)?;
    Ok(state.base.amount as u128)
}
