//! Authority-only: retune fee, liquidation, LTV, and interest parameters.
//!
//! Bounds match `InitializePool::validate_params`. Changes apply
//! prospectively: existing loans keep their stored `trigger_price_wad`
//! (so liq_ratio_bps changes don't retroactively re-bucket open loans);
//! interest accrual since `last_accrual_slot` is computed at the rate in
//! effect at the time of the next touch (open / repay / liquidation).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program_pack::Pack,
    pubkey::Pubkey,
    sysvar::Sysvar,
};
use spl_token_2022::state::Account as TokenAccount;

use crate::{
    error::LiquidityError,
    events::{Event, PoolSettingsUpdated},
    instructions::initialize_pool::validate_params,
    state::Pool,
};

#[allow(clippy::too_many_arguments)]
pub fn process_update_pool_settings(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    swap_fee_bps: u16,
    protocol_fee_bps: u16,
    liq_ratio_bps: u16,
    liq_penalty_bps: u16,
    max_ltv_bps: u16,
    interest_base_bps_per_year: u16,
    interest_slope1_bps_per_year: u16,
    interest_slope2_bps_per_year: u16,
    interest_kink_bps: u16,
) -> ProgramResult {
    let it = &mut accounts.iter();
    let pool_info = next_account_info(it)?;
    let vault_a_info = next_account_info(it)?;
    let vault_b_info = next_account_info(it)?;
    let authority_info = next_account_info(it)?;

    if !authority_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    if pool_info.owner != program_id {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }

    let mut pool = {
        let data = pool_info.try_borrow_data()?;
        Pool::try_from_slice(&data).map_err(|_| LiquidityError::AccountDataTooSmall)?
    };
    if !pool.is_initialized() {
        return Err(LiquidityError::NotInitialized.into());
    }
    if pool.is_authority_renounced() {
        return Err(LiquidityError::AuthorityRenounced.into());
    }
    if pool.authority != *authority_info.key {
        return Err(LiquidityError::InvalidAuthority.into());
    }
    if pool.vault_a != *vault_a_info.key || pool.vault_b != *vault_b_info.key {
        return Err(LiquidityError::InvalidPool.into());
    }

    validate_params(
        swap_fee_bps,
        protocol_fee_bps,
        liq_ratio_bps,
        liq_penalty_bps,
        max_ltv_bps,
        interest_base_bps_per_year,
        interest_slope1_bps_per_year,
        interest_slope2_bps_per_year,
        interest_kink_bps,
    )?;

    // Bump indexes BEFORE applying the new curve so the elapsed period is
    // capitalized at the previous rate.
    let clock = Clock::get()?;
    let real_a = read_amount(vault_a_info)?;
    let real_b = read_amount(vault_b_info)?;
    pool.bump_indexes(real_a, real_b, clock.slot)?;

    pool.swap_fee_bps = swap_fee_bps;
    pool.protocol_fee_bps = protocol_fee_bps;
    pool.liq_ratio_bps = liq_ratio_bps;
    pool.liq_penalty_bps = liq_penalty_bps;
    pool.max_ltv_bps = max_ltv_bps;
    pool.interest_base_bps_per_year = interest_base_bps_per_year;
    pool.interest_slope1_bps_per_year = interest_slope1_bps_per_year;
    pool.interest_slope2_bps_per_year = interest_slope2_bps_per_year;
    pool.interest_kink_bps = interest_kink_bps;
    pool.last_update_slot = clock.slot;
    {
        let mut data = pool_info.try_borrow_mut_data()?;
        pool.serialize(&mut &mut data[..])?;
    }

    msg!(
        "UpdatePoolSettings swap_fee={} prot_fee={} liq_ratio={} liq_pen={} max_ltv={} base={} s1={} s2={} kink={}",
        swap_fee_bps,
        protocol_fee_bps,
        liq_ratio_bps,
        liq_penalty_bps,
        max_ltv_bps,
        interest_base_bps_per_year,
        interest_slope1_bps_per_year,
        interest_slope2_bps_per_year,
        interest_kink_bps
    );
    PoolSettingsUpdated {
        pool: *pool_info.key,
        swap_fee_bps,
        protocol_fee_bps,
        liq_ratio_bps,
        liq_penalty_bps,
        max_ltv_bps,
        interest_base_bps_per_year,
        interest_slope1_bps_per_year,
        interest_slope2_bps_per_year,
        interest_kink_bps,
    }
    .emit();
    Ok(())
}

fn read_amount(info: &AccountInfo) -> Result<u128, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let acc = TokenAccount::unpack(&data).map_err(|_| LiquidityError::InvalidVault)?;
    Ok(acc.amount as u128)
}
