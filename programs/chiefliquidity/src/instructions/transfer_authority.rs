//! Authority-only: rotate or renounce the pool's `authority` field.
//!
//! Setting `new_authority = Pubkey::default()` permanently renounces the
//! authority — once renounced, no future TransferAuthority or
//! UpdatePoolSettings or ClaimProtocolFees can succeed against this pool.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    pubkey::Pubkey,
    sysvar::Sysvar,
};

use crate::{
    error::LiquidityError,
    events::{AuthorityTransferred, Event},
    state::Pool,
};

pub fn process_transfer_authority(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    new_authority: Pubkey,
) -> ProgramResult {
    let it = &mut accounts.iter();
    let pool_info = next_account_info(it)?;
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

    let was_renounce = new_authority == Pubkey::default();
    let old_authority = pool.authority;
    pool.authority = new_authority;
    pool.last_update_slot = Clock::get()?.slot;
    {
        let mut data = pool_info.try_borrow_mut_data()?;
        pool.serialize(&mut &mut data[..])?;
    }

    if was_renounce {
        msg!("TransferAuthority: authority renounced");
    } else {
        msg!("TransferAuthority: new={}", new_authority);
    }
    AuthorityTransferred {
        pool: *pool_info.key,
        old_authority,
        new_authority,
    }
    .emit();
    Ok(())
}
