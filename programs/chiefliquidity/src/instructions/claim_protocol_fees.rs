//! Drain accumulated protocol fees to the fixed protocol-fee recipient.
//!
//! This is a **permissionless crank**: anyone may call it (no signer or
//! authority is required). The accumulated `protocol_fees_a` / `protocol_fees_b`
//! are always routed to token accounts owned by [`PROTOCOL_FEE_RECIPIENT`] — the
//! caller only supplies those destination accounts, and the program verifies
//! each is owned by the fixed recipient before transferring.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    pubkey::Pubkey,
    sysvar::Sysvar,
};
use spl_token_2022::{
    extension::StateWithExtensions,
    state::{Account as TokenAccount, Mint},
};

use crate::{
    error::LiquidityError,
    events::{Event, ProtocolFeesClaimed},
    state::{validate_token_program_for_mint, Pool, POOL_SEED},
};

/// Fixed protocol-fee recipient (`23KPtJApAdwgo1ogjSLLUrx6ghy79ArNzJLeqMNhhiDj`).
///
/// `ClaimProtocolFees` is a permissionless crank; the accumulated fees always
/// route to token accounts owned by this address, regardless of who calls it.
pub const PROTOCOL_FEE_RECIPIENT: Pubkey = Pubkey::new_from_array([
    0x0f, 0x73, 0xa5, 0xb7, 0xfa, 0x25, 0xa1, 0xbc,
    0xf2, 0x49, 0x04, 0x9b, 0x55, 0xd9, 0x32, 0x09,
    0x3d, 0xdc, 0x94, 0xcc, 0xc0, 0xb0, 0xf6, 0xb3,
    0xb9, 0x17, 0xd0, 0x87, 0x6f, 0x1c, 0xe8, 0x86,
]);

pub fn process_claim_protocol_fees(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let it = &mut accounts.iter();

    let pool_info = next_account_info(it)?;
    let vault_a_info = next_account_info(it)?;
    let vault_b_info = next_account_info(it)?;
    let dest_a_info = next_account_info(it)?;
    let dest_b_info = next_account_info(it)?;
    let mint_a_info = next_account_info(it)?;
    let mint_b_info = next_account_info(it)?;
    let token_program_a_info = next_account_info(it)?;
    let token_program_b_info = next_account_info(it)?;

    validate_token_program_for_mint(token_program_a_info, mint_a_info)?;
    validate_token_program_for_mint(token_program_b_info, mint_b_info)?;
    if pool_info.owner != program_id {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }

    // Permissionless: no signer/authority gate. The only constraint is that the
    // fees land in token accounts owned by the fixed recipient — verified below.
    if read_token_owner(dest_a_info)? != PROTOCOL_FEE_RECIPIENT
        || read_token_owner(dest_b_info)? != PROTOCOL_FEE_RECIPIENT
    {
        return Err(LiquidityError::InvalidFeeRecipient.into());
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

    let amount_a = pool.protocol_fees_a;
    let amount_b = pool.protocol_fees_b;
    if amount_a == 0 && amount_b == 0 {
        msg!("ClaimProtocolFees: nothing accumulated");
        return Ok(());
    }

    let mint_a_decimals = read_mint_decimals(mint_a_info)?;
    let mint_b_decimals = read_mint_decimals(mint_b_info)?;

    let pool_seeds: &[&[u8]] = &[
        POOL_SEED,
        pool.mint_a.as_ref(),
        pool.mint_b.as_ref(),
        std::slice::from_ref(&pool.pool_bump),
    ];

    if amount_a > 0 {
        invoke_signed(
            &spl_token_2022::instruction::transfer_checked(
                token_program_a_info.key,
                vault_a_info.key,
                mint_a_info.key,
                dest_a_info.key,
                pool_info.key,
                &[],
                amount_a,
                mint_a_decimals,
            )?,
            &[
                vault_a_info.clone(),
                mint_a_info.clone(),
                dest_a_info.clone(),
                pool_info.clone(),
            ],
            &[pool_seeds],
        )?;
    }
    if amount_b > 0 {
        invoke_signed(
            &spl_token_2022::instruction::transfer_checked(
                token_program_b_info.key,
                vault_b_info.key,
                mint_b_info.key,
                dest_b_info.key,
                pool_info.key,
                &[],
                amount_b,
                mint_b_decimals,
            )?,
            &[
                vault_b_info.clone(),
                mint_b_info.clone(),
                dest_b_info.clone(),
                pool_info.clone(),
            ],
            &[pool_seeds],
        )?;
    }

    pool.protocol_fees_a = 0;
    pool.protocol_fees_b = 0;
    pool.last_update_slot = Clock::get()?.slot;
    let mut data = pool_info.try_borrow_mut_data()?;
    pool.serialize(&mut &mut data[..])?;

    msg!(
        "ClaimProtocolFees a={} b={} recipient={}",
        amount_a,
        amount_b,
        PROTOCOL_FEE_RECIPIENT
    );
    ProtocolFeesClaimed {
        pool: *pool_info.key,
        recipient: PROTOCOL_FEE_RECIPIENT,
        amount_a,
        amount_b,
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

/// Read the SPL token account's owner (the authority that controls it), used to
/// prove a fee destination belongs to the fixed recipient. Works for both SPL
/// Token and Token-2022 accounts (the base layout is shared).
fn read_token_owner(info: &AccountInfo) -> Result<Pubkey, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&data)
        .map_err(|_| LiquidityError::InvalidFeeRecipient)?;
    Ok(state.base.owner)
}
