//! Drain accumulated protocol fees to the caller. Gated on the program's
//! upgrade authority (read from the ProgramData account) — pools themselves
//! have no authority.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    bpf_loader_upgradeable,
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
    state::{validate_token_program_for_mint, Pool, POOL_SEED},
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
    let program_data_info = next_account_info(it)?;
    let token_program_a_info = next_account_info(it)?;
    let token_program_b_info = next_account_info(it)?;

    if !authority_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    validate_token_program_for_mint(token_program_a_info, mint_a_info)?;
    validate_token_program_for_mint(token_program_b_info, mint_b_info)?;
    if pool_info.owner != program_id {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }

    // Gate on the program's UPGRADE authority (pools have no authority of their
    // own). The ProgramData account for this program records the upgrade
    // authority; the signer must equal it. Verify the passed account is the
    // canonical ProgramData PDA and owned by the upgradeable loader first, so a
    // spoofed account can't grant access.
    let (expected_program_data, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::id());
    if *program_data_info.key != expected_program_data
        || *program_data_info.owner != bpf_loader_upgradeable::id()
    {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }
    // UpgradeableLoaderState::ProgramData bincode layout:
    //   [0..4]   u32 enum tag (== 3 for ProgramData)
    //   [4..12]  u64 last-deployed slot
    //   [12]     Option tag (1 = Some(upgrade_authority))
    //   [13..45] Pubkey upgrade_authority (present iff tag == 1)
    {
        let pd = program_data_info.try_borrow_data()?;
        if pd.len() < 45 || u32::from_le_bytes([pd[0], pd[1], pd[2], pd[3]]) != 3 {
            return Err(LiquidityError::InvalidAccountOwner.into());
        }
        if pd[12] != 1 {
            // Upgrade authority renounced → program is immutable → no claimant.
            return Err(LiquidityError::AuthorityRenounced.into());
        }
        let upgrade_authority =
            Pubkey::try_from(&pd[13..45]).map_err(|_| LiquidityError::InvalidAccountOwner)?;
        if upgrade_authority != *authority_info.key {
            return Err(LiquidityError::InvalidAuthority.into());
        }
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
