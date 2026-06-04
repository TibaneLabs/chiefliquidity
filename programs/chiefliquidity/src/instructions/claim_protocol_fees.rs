//! Authority-only: drain accumulated protocol fees from the vaults to the
//! authority's token accounts.

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
use spl_token_2022::{extension::StateWithExtensions, state::Mint};

use crate::{
    error::LiquidityError,
    events::{Event, ProtocolFeesClaimed},
    state::{is_valid_token_program, Pool, POOL_SEED},
};

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
    let authority_info = next_account_info(it)?;
    let token_program_info = next_account_info(it)?;

    if !authority_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    if !is_valid_token_program(token_program_info.key) {
        return Err(LiquidityError::InvalidTokenProgram.into());
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
                token_program_info.key,
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
                token_program_info.key,
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
        "ClaimProtocolFees a={} b={} authority={}",
        amount_a,
        amount_b,
        authority_info.key
    );
    ProtocolFeesClaimed {
        pool: *pool_info.key,
        authority: *authority_info.key,
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
