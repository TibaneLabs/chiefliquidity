//! Initialize a new (mint_a, mint_b) liquidity pool.
//!
//! Permissionless: any signer that pays rent can create a pool for any
//! validated mint pair. The signer becomes the pool's `authority` (which
//! controls fee-skim withdrawal); authority can be renounced later by
//! transferring it to `Pubkey::default()`.

use borsh::BorshSerialize;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};
use spl_token_2022::{
    extension::{
        permanent_delegate::PermanentDelegate, transfer_fee::TransferFeeConfig,
        transfer_hook::TransferHook, BaseStateWithExtensions, ExtensionType,
        StateWithExtensions,
    },
    state::Mint,
};

use crate::{
    error::LiquidityError,
    events::{Event, PoolInitialized},
    math::BPS_DENOM,
    state::{
        is_valid_token_program, CurveKind, Pool, LP_MINT_SEED, POOL_DISCRIMINATOR,
        POOL_SEED, VAULT_A_SEED, VAULT_B_SEED,
    },
};

/// Decimals for the pool's LP token mint. Fixed at 9 (SOL convention).
pub const LP_MINT_DECIMALS: u8 = 9;

// ---- Parameter bounds ----

pub const MIN_LIQ_RATIO_BPS: u16 = 10_100; // 101%
pub const MAX_LIQ_RATIO_BPS: u16 = 30_000; // 300%
pub const MAX_SWAP_FEE_BPS: u16 = 1_000; // 10%
pub const MIN_LTV_BPS: u16 = 100; // 1%

// Interest model bounds (all in bps-per-year, except kink which is bps of utilization).
pub const MAX_INTEREST_BASE_BPS: u16 = 10_000; // 100% APR base
pub const MAX_INTEREST_SLOPE1_BPS: u16 = 10_000; // 100% APR at kink
pub const MAX_INTEREST_SLOPE2_BPS: u16 = 65_000; // ~650% APR over kink
pub const MIN_KINK_BPS: u16 = 100; // 1% utilization
pub const MAX_KINK_BPS: u16 = 9_900; // 99% utilization

/// Initialize a new liquidity pool.
///
/// Accounts:
/// 0. `[writable]` Pool PDA — `["pool", mint_a, mint_b]`
/// 1. `[]`        Mint A
/// 2. `[]`        Mint B
/// 3. `[writable]` Vault A PDA — `["vault_a", pool]`
/// 4. `[writable]` Vault B PDA — `["vault_b", pool]`
/// 5. `[writable]` LP mint PDA — `["lp_mint", pool]`
/// 6. `[writable, signer]` Authority/payer
/// 7. `[]`        System program
/// 8. `[]`        Token program (SPL Token or Token 2022)
/// 9. `[]`        Rent sysvar
#[allow(clippy::too_many_arguments)]
pub fn process_initialize_pool(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    swap_fee_bps: u16,
    protocol_fee_bps: u16,
    liq_ratio_bps: u16,
    max_ltv_bps: u16,
    interest_base_bps_per_year: u16,
    interest_slope1_bps_per_year: u16,
    interest_slope2_bps_per_year: u16,
    interest_kink_bps: u16,
) -> ProgramResult {
    let it = &mut accounts.iter();

    let pool_info = next_account_info(it)?;
    let mint_a_info = next_account_info(it)?;
    let mint_b_info = next_account_info(it)?;
    let vault_a_info = next_account_info(it)?;
    let vault_b_info = next_account_info(it)?;
    let lp_mint_info = next_account_info(it)?;
    let authority_info = next_account_info(it)?;
    let system_program_info = next_account_info(it)?;
    let token_program_info = next_account_info(it)?;
    let rent_sysvar_info = next_account_info(it)?;

    // ---- Signer / token program checks ----

    if !authority_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    if !is_valid_token_program(token_program_info.key) {
        return Err(LiquidityError::InvalidTokenProgram.into());
    }

    // ---- Mint pair validation (canonical ordering, both same token program) ----

    if mint_a_info.key == mint_b_info.key {
        return Err(LiquidityError::MintsMustDiffer.into());
    }
    if mint_a_info.key.as_ref() >= mint_b_info.key.as_ref() {
        return Err(LiquidityError::MintsNotSorted.into());
    }
    if *mint_a_info.owner != *token_program_info.key
        || *mint_b_info.owner != *token_program_info.key
    {
        return Err(LiquidityError::InvalidMintProgram.into());
    }

    validate_mint_extensions(mint_a_info, token_program_info.key)?;
    validate_mint_extensions(mint_b_info, token_program_info.key)?;

    // ---- Parameter bounds ----

    validate_params(
        swap_fee_bps,
        protocol_fee_bps,
        liq_ratio_bps,
        max_ltv_bps,
        interest_base_bps_per_year,
        interest_slope1_bps_per_year,
        interest_slope2_bps_per_year,
        interest_kink_bps,
    )?;

    // ---- PDA derivations ----

    let (expected_pool, pool_bump) =
        Pool::derive_pda(mint_a_info.key, mint_b_info.key, program_id);
    if *pool_info.key != expected_pool {
        return Err(LiquidityError::InvalidPDA.into());
    }
    let (expected_vault_a, vault_a_bump) =
        Pool::derive_vault_a_pda(pool_info.key, program_id);
    if *vault_a_info.key != expected_vault_a {
        return Err(LiquidityError::InvalidPDA.into());
    }
    let (expected_vault_b, vault_b_bump) =
        Pool::derive_vault_b_pda(pool_info.key, program_id);
    if *vault_b_info.key != expected_vault_b {
        return Err(LiquidityError::InvalidPDA.into());
    }
    let (expected_lp_mint, lp_mint_bump) =
        Pool::derive_lp_mint_pda(pool_info.key, program_id);
    if *lp_mint_info.key != expected_lp_mint {
        return Err(LiquidityError::InvalidPDA.into());
    }

    // Reject re-initialization.
    if !pool_info.data_is_empty() {
        return Err(LiquidityError::AlreadyInitialized.into());
    }

    let rent = Rent::from_account_info(rent_sysvar_info)?;
    let clock = Clock::get()?;

    // ---- Create Pool account ----

    let pool_seeds = &[
        POOL_SEED,
        mint_a_info.key.as_ref(),
        mint_b_info.key.as_ref(),
        &[pool_bump],
    ];
    let pool_rent = rent.minimum_balance(Pool::LEN);
    invoke_signed(
        &system_instruction::create_account(
            authority_info.key,
            pool_info.key,
            pool_rent,
            Pool::LEN as u64,
            program_id,
        ),
        &[
            authority_info.clone(),
            pool_info.clone(),
            system_program_info.clone(),
        ],
        &[pool_seeds],
    )?;

    // ---- Create Vault A & B (SPL token accounts owned by Pool PDA) ----

    create_vault(
        program_id,
        token_program_info,
        system_program_info,
        authority_info,
        pool_info,
        mint_a_info,
        vault_a_info,
        VAULT_A_SEED,
        vault_a_bump,
        &rent,
    )?;
    create_vault(
        program_id,
        token_program_info,
        system_program_info,
        authority_info,
        pool_info,
        mint_b_info,
        vault_b_info,
        VAULT_B_SEED,
        vault_b_bump,
        &rent,
    )?;

    // ---- Create LP Mint (SPL mint, mint authority = Pool PDA) ----

    create_lp_mint(
        program_id,
        token_program_info,
        system_program_info,
        authority_info,
        pool_info,
        lp_mint_info,
        lp_mint_bump,
        &rent,
    )?;

    // ---- Persist Pool state ----

    let pool = Pool {
        discriminator: POOL_DISCRIMINATOR,
        mint_a: *mint_a_info.key,
        mint_b: *mint_b_info.key,
        vault_a: *vault_a_info.key,
        vault_b: *vault_b_info.key,
        lp_mint: *lp_mint_info.key,
        authority: *authority_info.key,
        pool_bump,
        vault_a_bump,
        vault_b_bump,
        lp_mint_bump,
        total_debt_a: 0,
        total_debt_b: 0,
        total_collateral_a: 0,
        total_collateral_b: 0,
        curve_kind: CurveKind::Cpmm as u8,
        swap_fee_bps,
        protocol_fee_bps,
        _curve_pad: [0; 3],
        liq_ratio_bps,
        max_ltv_bps,
        _lending_pad: [0; 2],
        interest_base_bps_per_year,
        interest_slope1_bps_per_year,
        interest_slope2_bps_per_year,
        interest_kink_bps,
        borrow_index_a_wad: crate::math::WAD,
        borrow_index_b_wad: crate::math::WAD,
        last_index_update_slot: clock.slot,
        open_loans: 0,
        next_loan_nonce: 0,
        last_update_slot: clock.slot,
        protocol_fees_a: 0,
        protocol_fees_b: 0,
        band_bitmap_fall: [0; 16],
        band_bitmap_rise: [0; 16],
        _reserved: [0; 32],
    };
    let mut data = pool_info.try_borrow_mut_data()?;
    pool.serialize(&mut &mut data[..])?;

    msg!(
        "Initialized pool {} ({}/{})",
        pool_info.key,
        mint_a_info.key,
        mint_b_info.key
    );
    PoolInitialized {
        pool: *pool_info.key,
        mint_a: *mint_a_info.key,
        mint_b: *mint_b_info.key,
        authority: *authority_info.key,
        swap_fee_bps,
        protocol_fee_bps,
        liq_ratio_bps,
        max_ltv_bps,
        interest_base_bps_per_year,
        interest_slope1_bps_per_year,
        interest_slope2_bps_per_year,
        interest_kink_bps,
    }
    .emit();
    Ok(())
}

// ---- helpers ----

#[allow(clippy::too_many_arguments)]
pub fn validate_params(
    swap_fee_bps: u16,
    protocol_fee_bps: u16,
    liq_ratio_bps: u16,
    max_ltv_bps: u16,
    interest_base_bps_per_year: u16,
    interest_slope1_bps_per_year: u16,
    interest_slope2_bps_per_year: u16,
    interest_kink_bps: u16,
) -> ProgramResult {
    if swap_fee_bps > MAX_SWAP_FEE_BPS {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }
    if protocol_fee_bps > swap_fee_bps {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }
    if liq_ratio_bps < MIN_LIQ_RATIO_BPS || liq_ratio_bps > MAX_LIQ_RATIO_BPS {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }
    if max_ltv_bps < MIN_LTV_BPS {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }
    // Loans must open below the liquidation threshold:
    //   collateral / debt > liq_ratio   ⇒   debt/collateral < 1/liq_ratio
    //   max_ltv < BPS_DENOM^2 / liq_ratio
    let max_safe_ltv = (BPS_DENOM * BPS_DENOM) / liq_ratio_bps as u128;
    if (max_ltv_bps as u128) >= max_safe_ltv {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }

    // Interest model bounds
    if interest_base_bps_per_year > MAX_INTEREST_BASE_BPS {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }
    if interest_slope1_bps_per_year > MAX_INTEREST_SLOPE1_BPS {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }
    if interest_slope2_bps_per_year > MAX_INTEREST_SLOPE2_BPS {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }
    if interest_kink_bps < MIN_KINK_BPS || interest_kink_bps > MAX_KINK_BPS {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }
    Ok(())
}

/// Reject Token-2022 mints with extensions that would break vault accounting
/// or enable theft. SPL Token mints have no extensions, so this is a no-op
/// for them.
fn validate_mint_extensions(mint_info: &AccountInfo, token_program: &Pubkey) -> ProgramResult {
    if *token_program != spl_token_2022::id() {
        return Ok(());
    }
    let data = mint_info.try_borrow_data()?;
    let state = StateWithExtensions::<Mint>::unpack(&data)?;

    if state.get_extension::<TransferFeeConfig>().is_ok() {
        msg!("mint {} has TransferFee extension — rejected", mint_info.key);
        return Err(LiquidityError::UnsupportedMintExtension.into());
    }
    if state.get_extension::<PermanentDelegate>().is_ok() {
        msg!(
            "mint {} has PermanentDelegate extension — rejected",
            mint_info.key
        );
        return Err(LiquidityError::UnsupportedMintExtension.into());
    }
    if state.get_extension::<TransferHook>().is_ok() {
        msg!(
            "mint {} has TransferHook extension — rejected",
            mint_info.key
        );
        return Err(LiquidityError::UnsupportedMintExtension.into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_vault<'a>(
    program_id: &Pubkey,
    token_program_info: &AccountInfo<'a>,
    system_program_info: &AccountInfo<'a>,
    payer_info: &AccountInfo<'a>,
    pool_info: &AccountInfo<'a>,
    mint_info: &AccountInfo<'a>,
    vault_info: &AccountInfo<'a>,
    seed: &[u8],
    bump: u8,
    rent: &Rent,
) -> ProgramResult {
    let _ = program_id;
    let seeds = &[seed, pool_info.key.as_ref(), &[bump]];

    let vault_size = if *token_program_info.key == spl_token_2022::id() {
        ExtensionType::try_calculate_account_len::<spl_token_2022::state::Account>(&[])?
    } else {
        spl_token_2022::state::Account::LEN
    };
    let vault_rent = rent.minimum_balance(vault_size);

    invoke_signed(
        &system_instruction::create_account(
            payer_info.key,
            vault_info.key,
            vault_rent,
            vault_size as u64,
            token_program_info.key,
        ),
        &[
            payer_info.clone(),
            vault_info.clone(),
            system_program_info.clone(),
        ],
        &[seeds],
    )?;

    invoke_signed(
        &spl_token_2022::instruction::initialize_account3(
            token_program_info.key,
            vault_info.key,
            mint_info.key,
            pool_info.key, // Pool PDA owns the vault
        )?,
        &[vault_info.clone(), mint_info.clone()],
        &[seeds],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_lp_mint<'a>(
    program_id: &Pubkey,
    token_program_info: &AccountInfo<'a>,
    system_program_info: &AccountInfo<'a>,
    payer_info: &AccountInfo<'a>,
    pool_info: &AccountInfo<'a>,
    lp_mint_info: &AccountInfo<'a>,
    bump: u8,
    rent: &Rent,
) -> ProgramResult {
    let _ = program_id;
    let seeds = &[LP_MINT_SEED, pool_info.key.as_ref(), &[bump]];

    let mint_size = if *token_program_info.key == spl_token_2022::id() {
        ExtensionType::try_calculate_account_len::<spl_token_2022::state::Mint>(&[])?
    } else {
        spl_token_2022::state::Mint::LEN
    };
    let mint_rent = rent.minimum_balance(mint_size);

    invoke_signed(
        &system_instruction::create_account(
            payer_info.key,
            lp_mint_info.key,
            mint_rent,
            mint_size as u64,
            token_program_info.key,
        ),
        &[
            payer_info.clone(),
            lp_mint_info.clone(),
            system_program_info.clone(),
        ],
        &[seeds],
    )?;

    invoke_signed(
        &spl_token_2022::instruction::initialize_mint2(
            token_program_info.key,
            lp_mint_info.key,
            pool_info.key,       // mint authority = pool PDA
            Some(pool_info.key), // freeze authority = pool PDA
            LP_MINT_DECIMALS,
        )?,
        &[lp_mint_info.clone()],
        &[seeds],
    )?;
    Ok(())
}
