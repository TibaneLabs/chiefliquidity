//! Borrower-callable: reclaim rent from a tombstoned (liquidated) loan.
//!
//! Swap-driven liquidation in `swap.rs` deliberately leaves a `Loan` account
//! with `status = STATUS_LIQUIDATED` (and its `LoanLink` data zeroed) so the
//! original borrower can later prove ownership and recover the rent.

use borsh::BorshDeserialize;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    pubkey::Pubkey,
};

use crate::{
    error::LiquidityError,
    events::{Event, LiquidatedRentClaimed},
    state::{Loan, LoanLink},
};

pub fn process_claim_liquidated_rent(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let it = &mut accounts.iter();
    let loan_info = next_account_info(it)?;
    let loan_link_info = next_account_info(it)?;
    let borrower_info = next_account_info(it)?;

    if !borrower_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    if loan_info.owner != program_id || loan_link_info.owner != program_id {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }

    // Loan tombstone must still hold its discriminator and borrower; status
    // must be LIQUIDATED.
    let loan = {
        let data = loan_info.try_borrow_data()?;
        Loan::try_from_slice(&data).map_err(|_| LiquidityError::AccountDataTooSmall)?
    };
    if !loan.is_initialized() {
        return Err(LiquidityError::NotInitialized.into());
    }
    if loan.status != Loan::STATUS_LIQUIDATED {
        return Err(LiquidityError::LoanNotLiquidatable.into());
    }
    if loan.borrower != *borrower_info.key {
        return Err(LiquidityError::InvalidAuthority.into());
    }

    // Verify loan_link is the canonical PDA for this loan.
    let (expected_link, _) =
        LoanLink::derive_pda(&loan.pool, loan_info.key, program_id);
    if expected_link != *loan_link_info.key {
        return Err(LiquidityError::InvalidPDA.into());
    }

    // Drain link first — its data is already zeroed from swap.rs.
    drain(loan_link_info, borrower_info)?;
    // Drain loan — also wipe its data so the runtime garbage-collects.
    {
        let mut data = loan_info.try_borrow_mut_data()?;
        for byte in data.iter_mut() {
            *byte = 0;
        }
    }
    drain(loan_info, borrower_info)?;

    msg!(
        "ClaimLiquidatedRent loan={} borrower={}",
        loan_info.key,
        borrower_info.key
    );
    LiquidatedRentClaimed {
        pool: loan.pool,
        loan: *loan_info.key,
        borrower: *borrower_info.key,
    }
    .emit();
    Ok(())
}

fn drain<'a>(account: &AccountInfo<'a>, dest: &AccountInfo<'a>) -> ProgramResult {
    let lamports = account.lamports();
    if lamports == 0 {
        return Ok(());
    }
    **account.try_borrow_mut_lamports()? = 0;
    **dest.try_borrow_mut_lamports()? = dest
        .lamports()
        .checked_add(lamports)
        .ok_or(LiquidityError::MathOverflow)?;
    Ok(())
}
