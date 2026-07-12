//! Initialize a new (mint_a, mint_b) liquidity pool.
//!
//! Permissionless: any signer that pays rent can create a pool for any
//! validated mint pair. Pools are authority-less by construction — the stored
//! `authority` is hardcoded to `Pubkey::default()`, so creating a pool grants
//! no special rights and there is no pool admin to retune or drain it. Fee
//! redemption is a permissionless crank that pays a fixed recipient, not the pool.

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
    extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    state::Mint,
};

use crate::{
    error::LiquidityError,
    events::{Event, PoolInitialized},
    math::BPS_DENOM,
    state::{
        validate_token_program_for_mint, CurveKind, Pool, LP_MINT_SEED, POOL_DISCRIMINATOR,
        POOL_SEED, VAULT_A_SEED, VAULT_B_SEED,
    },
};

/// Decimals for the pool's LP token mint. Fixed at 9 (SOL convention).
pub const LP_MINT_DECIMALS: u8 = 9;

// ---- Fixed pool economics (immutable; every pool gets exactly these) ----
//
// Pools are authority-less and non-configurable: these constants ARE the pool
// parameters. `InitializePool` bakes them in and there is no instruction to
// change them afterward. The compile-time asserts below reproduce the safety
// bounds the old runtime `validate_params` enforced, so an unsafe edit fails
// `cargo build` instead of shipping a broken pool.

pub const SWAP_FEE_BPS: u16 = 30; // 0.30%
pub const PROTOCOL_FEE_BPS: u16 = 5; // protocol's cut of the swap fee (of the 30)
pub const LIQ_RATIO_BPS: u16 = 11_000; // 110%
pub const MAX_LTV_BPS: u16 = 8_000; // 80%
pub const INTEREST_BASE_BPS_PER_YEAR: u16 = 0; // base APR at zero utilization
pub const INTEREST_SLOPE1_BPS_PER_YEAR: u16 = 400; // +4% APR by the kink
pub const INTEREST_SLOPE2_BPS_PER_YEAR: u16 = 30_000; // +300% APR over the kink
pub const INTEREST_KINK_BPS: u16 = 8_000; // kink at 80% utilization

const _: () = {
    assert!(SWAP_FEE_BPS <= 1_000); // fee <= 10%
    assert!(PROTOCOL_FEE_BPS <= SWAP_FEE_BPS); // protocol cut <= swap fee
    assert!(LIQ_RATIO_BPS >= 10_100 && LIQ_RATIO_BPS <= 30_000); // 101%..=300%
    assert!(MAX_LTV_BPS >= 100); // >= 1%
    // Loans must open strictly below the liquidation threshold:
    //   max_ltv < BPS_DENOM^2 / liq_ratio
    assert!((MAX_LTV_BPS as u128) * (LIQ_RATIO_BPS as u128) < BPS_DENOM * BPS_DENOM);
    assert!(INTEREST_BASE_BPS_PER_YEAR <= 10_000);
    assert!(INTEREST_SLOPE1_BPS_PER_YEAR <= 10_000);
    assert!(INTEREST_SLOPE2_BPS_PER_YEAR <= 65_000);
    assert!(INTEREST_KINK_BPS >= 100 && INTEREST_KINK_BPS <= 9_900); // 1%..=99%
};

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
/// 8. `[]`        Token program for mint A (SPL Token or Token 2022)
/// 9. `[]`        Token program for mint B (SPL Token or Token 2022)
/// 10. `[]`       Rent sysvar
///
/// The two token programs may differ: e.g. a Token-2022 mint paired with a
/// legacy SPL mint like wSOL. Vault A / the LP mint are created under program A;
/// vault B under program B.
pub fn process_initialize_pool(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
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
    let token_program_a_info = next_account_info(it)?;
    let token_program_b_info = next_account_info(it)?;
    let rent_sysvar_info = next_account_info(it)?;

    // ---- Signer / per-side token program checks ----

    if !authority_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    // Each side's token program must be supported AND own that side's mint.
    validate_token_program_for_mint(token_program_a_info, mint_a_info)?;
    validate_token_program_for_mint(token_program_b_info, mint_b_info)?;

    // ---- Mint pair validation (canonical ordering) ----

    if mint_a_info.key == mint_b_info.key {
        return Err(LiquidityError::MintsMustDiffer.into());
    }
    if mint_a_info.key.as_ref() >= mint_b_info.key.as_ref() {
        return Err(LiquidityError::MintsNotSorted.into());
    }

    validate_mint_extensions(mint_a_info, token_program_a_info.key)?;
    validate_mint_extensions(mint_b_info, token_program_b_info.key)?;

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
        token_program_a_info,
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
        token_program_b_info,
        system_program_info,
        authority_info,
        pool_info,
        mint_b_info,
        vault_b_info,
        VAULT_B_SEED,
        vault_b_bump,
        &rent,
    )?;

    // ---- Create LP Mint (created under mint A's token program) ----

    create_lp_mint(
        program_id,
        token_program_a_info,
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
        // Authority-less: no one can retune, drain via a pool authority, or
        // rotate. Fee redemption is a permissionless crank to a fixed recipient.
        authority: Pubkey::default(),
        pool_bump,
        vault_a_bump,
        vault_b_bump,
        lp_mint_bump,
        total_debt_a: 0,
        total_debt_b: 0,
        total_collateral_a: 0,
        total_collateral_b: 0,
        curve_kind: CurveKind::Cpmm as u8,
        swap_fee_bps: SWAP_FEE_BPS,
        protocol_fee_bps: PROTOCOL_FEE_BPS,
        _curve_pad: [0; 3],
        liq_ratio_bps: LIQ_RATIO_BPS,
        max_ltv_bps: MAX_LTV_BPS,
        _lending_pad: [0; 2],
        interest_base_bps_per_year: INTEREST_BASE_BPS_PER_YEAR,
        interest_slope1_bps_per_year: INTEREST_SLOPE1_BPS_PER_YEAR,
        interest_slope2_bps_per_year: INTEREST_SLOPE2_BPS_PER_YEAR,
        interest_kink_bps: INTEREST_KINK_BPS,
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
        authority: Pubkey::default(),
        swap_fee_bps: SWAP_FEE_BPS,
        protocol_fee_bps: PROTOCOL_FEE_BPS,
        liq_ratio_bps: LIQ_RATIO_BPS,
        max_ltv_bps: MAX_LTV_BPS,
        interest_base_bps_per_year: INTEREST_BASE_BPS_PER_YEAR,
        interest_slope1_bps_per_year: INTEREST_SLOPE1_BPS_PER_YEAR,
        interest_slope2_bps_per_year: INTEREST_SLOPE2_BPS_PER_YEAR,
        interest_kink_bps: INTEREST_KINK_BPS,
    }
    .emit();
    Ok(())
}

// ---- helpers ----

/// Token-2022 mint extensions the pool can safely custody. This is an
/// **allowlist**: a mint carrying *any* extension not in this set is rejected,
/// so unknown / future / dangerous extensions fail closed rather than slipping
/// through a denylist.
///
/// Permitted (none affect transferability or the raw amounts the vaults track):
///   - `MintCloseAuthority`  — can only close at zero supply, impossible while a
///     pool holds the token.
///   - `InterestBearingConfig` — scales only the *UI* amount; raw amounts, and
///     therefore all vault/AMM accounting, are unchanged.
///   - metadata extensions (`MetadataPointer`, `TokenMetadata`, and the token
///     group/member set) — descriptive only.
///
/// Deliberately NOT allowed (break accounting, transferability, or enable
/// theft): `TransferFeeConfig`, `PermanentDelegate`, `TransferHook`,
/// `ConfidentialTransfer*`, `DefaultAccountState` (could default-freeze the
/// vault), `NonTransferable`, and anything else.
///
/// Note: classic SPL Token mints (USDC, USDT, wSOL, …) have no extensions and
/// take the early return below, so they are always accepted.
const MINT_EXTENSION_ALLOWLIST: &[ExtensionType] = &[
    ExtensionType::MintCloseAuthority,
    ExtensionType::InterestBearingConfig,
    ExtensionType::MetadataPointer,
    ExtensionType::TokenMetadata,
    ExtensionType::GroupPointer,
    ExtensionType::TokenGroup,
    ExtensionType::GroupMemberPointer,
    ExtensionType::TokenGroupMember,
];

/// Reject Token-2022 mints carrying any non-allowlisted extension. SPL Token
/// mints have no extensions, so this is a no-op for them.
fn validate_mint_extensions(mint_info: &AccountInfo, token_program: &Pubkey) -> ProgramResult {
    if *token_program != spl_token_2022::id() {
        return Ok(());
    }
    let data = mint_info.try_borrow_data()?;
    let state = StateWithExtensions::<Mint>::unpack(&data)?;

    for ext in state.get_extension_types()? {
        if !MINT_EXTENSION_ALLOWLIST.contains(&ext) {
            msg!(
                "mint {} has non-allowlisted extension {:?} — rejected",
                mint_info.key,
                ext
            );
            return Err(LiquidityError::UnsupportedMintExtension.into());
        }
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
            pool_info.key, // mint authority = pool PDA
            None,          // no freeze authority — LP tokens can never be frozen
            LP_MINT_DECIMALS,
        )?,
        std::slice::from_ref(lp_mint_info),
        &[seeds],
    )?;
    Ok(())
}
