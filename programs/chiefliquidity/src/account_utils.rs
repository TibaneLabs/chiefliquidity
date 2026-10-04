//! Shared account-creation helpers.
//!
//! Every account this program creates lives at a predictable PDA (pool for a
//! mint pair, its vaults / LP mint, a band `(pool, dir, band_id)`, a loan
//! `(pool, borrower, nonce)`). `system_instruction::create_account` refuses a
//! target that already holds lamports, so anyone could permanently block the
//! creation of such an address by "pre-funding" it with a dust transfer.
//! `create_pda_account` tolerates that: it tops up only the missing rent and
//! then `allocate`s + `assign`s the PDA instead.

use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    pubkey::Pubkey,
    rent::Rent,
    system_instruction, system_program,
};

use crate::error::LiquidityError;

/// Create a rent-exempt PDA account of `space` bytes owned by `owner`, with
/// `payer` funding the rent and `signer_seeds` signing for `target`.
///
/// - `target` has no lamports → plain `create_account`.
/// - `target` was pre-funded (system-owned, no data) → transfer only the rent
///   still missing, then `allocate` + `assign` (both PDA-signed).
/// - anything else (already owned by a program, or carrying data) is an
///   existing account → `AlreadyInitialized`.
pub fn create_pda_account<'a>(
    payer: &AccountInfo<'a>,
    target: &AccountInfo<'a>,
    system_program_info: &AccountInfo<'a>,
    space: usize,
    owner: &Pubkey,
    signer_seeds: &[&[u8]],
    rent: &Rent,
) -> ProgramResult {
    let required = rent.minimum_balance(space);
    let current = target.lamports();

    if current == 0 {
        return invoke_signed(
            &system_instruction::create_account(
                payer.key,
                target.key,
                required,
                space as u64,
                owner,
            ),
            &[payer.clone(), target.clone(), system_program_info.clone()],
            &[signer_seeds],
        );
    }

    // Pre-funded. Only a bare system account (lamports, nothing else) may be
    // adopted; a program-owned or data-carrying account is a live account.
    if *target.owner != system_program::id() || !target.data_is_empty() {
        msg!("account {} already exists", target.key);
        return Err(LiquidityError::AlreadyInitialized.into());
    }

    let missing = required.saturating_sub(current);
    if missing > 0 {
        invoke(
            &system_instruction::transfer(payer.key, target.key, missing),
            &[payer.clone(), target.clone(), system_program_info.clone()],
        )?;
    }
    invoke_signed(
        &system_instruction::allocate(target.key, space as u64),
        &[target.clone(), system_program_info.clone()],
        &[signer_seeds],
    )?;
    invoke_signed(
        &system_instruction::assign(target.key, owner),
        &[target.clone(), system_program_info.clone()],
        &[signer_seeds],
    )
}

/// True iff `info` is owned by `program_id`, is exactly `len` bytes long and
/// every byte is zero — i.e. a closed account of ours whose lamports were
/// topped back up within the closing transaction ("revived"), leaving it
/// program-owned but with no state.
pub fn is_zeroed_program_account(info: &AccountInfo, program_id: &Pubkey, len: usize) -> bool {
    if info.owner != program_id || info.data_len() != len {
        return false;
    }
    match info.try_borrow_data() {
        Ok(data) => data.iter().all(|&b| b == 0),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_account<R>(owner: Pubkey, data: &mut [u8], f: impl FnOnce(&AccountInfo) -> R) -> R {
        let key = Pubkey::new_unique();
        let mut lamports = 1_000_000u64;
        let info = AccountInfo::new(&key, false, true, &mut lamports, data, &owner, false, 0);
        f(&info)
    }

    #[test]
    fn zeroed_program_account_detected() {
        let program_id = Pubkey::new_unique();
        let mut data = [0u8; 88];
        assert!(with_account(program_id, &mut data, |i| {
            is_zeroed_program_account(i, &program_id, 88)
        }));
    }

    #[test]
    fn nonzero_data_is_not_zeroed() {
        let program_id = Pubkey::new_unique();
        let mut data = [0u8; 88];
        data[87] = 1;
        assert!(!with_account(program_id, &mut data, |i| {
            is_zeroed_program_account(i, &program_id, 88)
        }));
    }

    #[test]
    fn wrong_owner_or_len_is_not_zeroed() {
        let program_id = Pubkey::new_unique();
        let mut data = [0u8; 88];
        assert!(!with_account(Pubkey::new_unique(), &mut data, |i| {
            is_zeroed_program_account(i, &program_id, 88)
        }));
        assert!(!with_account(program_id, &mut data, |i| {
            is_zeroed_program_account(i, &program_id, 210)
        }));
        let mut empty: [u8; 0] = [];
        assert!(!with_account(program_id, &mut empty, |i| {
            is_zeroed_program_account(i, &program_id, 88)
        }));
    }
}
