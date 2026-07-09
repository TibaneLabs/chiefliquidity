//! ChiefLiquidity: liquidation-aware AMM lending protocol.
//!
//! Each pool holds two arbitrary SPL tokens, accepts LP deposits, accepts
//! collateralized borrows of either side against the other, and executes
//! swaps against a post-liquidation reserve state — see `DESIGN.md`.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, msg,
    program_error::ProgramError, pubkey::Pubkey,
};

pub mod error;
pub mod events;
pub mod instructions;
pub mod math;
pub mod state;

use instructions::*;

// Canonical program ID (vanity address ending in "cHieF"). The matching
// keypair lives at ~/.config/solana/chiefliquidity-program.json and is used
// only for the initial deploy; upgrades are gated by the upgrade authority.
solana_program::declare_id!("ChiefQnUMyz7V1U9odcoxCar66ngVZn1wXFDecnN7yQw");

/// Program instructions.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub enum LiquidityInstruction {
    /// Initialize a new (mint_a, mint_b) pool. Mints must be sorted; the
    /// program enforces `mint_a < mint_b` so the pool PDA is canonical.
    ///
    /// Pools are **immutable and authority-less**: every economic parameter
    /// (fees, liquidation ratio, max LTV, interest curve) is a fixed program
    /// constant (see `initialize_pool.rs`), the pool's `authority` is set to
    /// `Pubkey::default()`, and there is no instruction to change any of it.
    /// The instruction therefore takes **no arguments**; the creator gains no
    /// special rights.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account (PDA: ["pool", mint_a, mint_b])
    /// 1. `[]` Mint A
    /// 2. `[]` Mint B
    /// 3. `[writable]` Vault A (PDA: ["vault_a", pool])
    /// 4. `[writable]` Vault B (PDA: ["vault_b", pool])
    /// 5. `[writable]` LP mint (PDA: ["lp_mint", pool])
    /// 6. `[writable, signer]` Payer (funds rent only; NOT recorded as authority)
    /// 7. `[]` System program
    /// 8. `[]` Token program for mint A (owns mint A + the LP mint)
    /// 9. `[]` Token program for mint B (owns mint B)
    /// 10. `[]` Rent sysvar
    ///
    /// Programs 8 and 9 may differ (e.g. a Token-2022 mint paired with legacy wSOL).
    InitializePool,

    /// Deposit `amount_a_max` of A and `amount_b_max` of B (or proportional
    /// fraction thereof) and receive LP tokens.
    ///
    /// First deposit: takes both maxes verbatim and mints `sqrt(a*b)` LP.
    /// Subsequent: takes the larger ratio-matched fraction and mints
    /// `min(a/A, b/B) * lp_supply`.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool
    /// 1. `[writable]` Vault A
    /// 2. `[writable]` Vault B
    /// 3. `[writable]` LP mint
    /// 4. `[writable]` User token account A (source)
    /// 5. `[writable]` User token account B (source)
    /// 6. `[writable]` User LP token account (destination)
    /// 7. `[signer]`   User
    /// 8. `[]`         Mint A
    /// 9. `[]`         Mint B
    /// 10. `[]`        Token program for mint A (also mints the LP token)
    /// 11. `[]`        Token program for mint B
    AddLiquidity {
        amount_a_max: u64,
        amount_b_max: u64,
        min_lp_out: u64,
    },

    /// Burn `lp_amount` of LP and receive proportional shares of accounted
    /// reserves. Reverts if real-reserve balances cannot cover the
    /// withdrawal (e.g. pool is heavily lent out).
    ///
    /// Accounts:
    /// 0. `[writable]` Pool
    /// 1. `[writable]` Vault A
    /// 2. `[writable]` Vault B
    /// 3. `[writable]` LP mint
    /// 4. `[writable]` User token account A (destination)
    /// 5. `[writable]` User token account B (destination)
    /// 6. `[writable]` User LP token account (source)
    /// 7. `[signer]`   User
    /// 8. `[]`         Mint A
    /// 9. `[]`         Mint B
    /// 10. `[]`        Token program for mint A (also burns the LP token)
    /// 11. `[]`        Token program for mint B
    RemoveLiquidity {
        lp_amount: u64,
        min_a_out: u64,
        min_b_out: u64,
    },

    /// Open a collateralized loan. Caller specifies the side (which token is
    /// collateral, which is debt), the amounts, and the loan nonce (which
    /// must equal `pool.next_loan_nonce`). Program computes the trigger
    /// price and band id from the supplied amounts and `liq_ratio_bps`,
    /// allocates the `LoanIndexBand` on first use, and increments its
    /// membership `count`.
    ///
    /// LTV check: debt_value / collateral_value ≤ `pool.max_ltv_bps`, with
    /// values converted via the pool's accounted mid-price.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool
    /// 1. `[writable]` Vault A
    /// 2. `[writable]` Vault B
    /// 3. `[writable]` Borrower's token account A
    /// 4. `[writable]` Borrower's token account B
    /// 5. `[]`         Mint A
    /// 6. `[]`         Mint B
    /// 7. `[writable, signer]` Borrower (also the rent payer for new accounts)
    /// 8. `[writable]` Loan PDA — `["loan", pool, borrower, nonce_le]`
    /// 9. `[writable]` Band PDA — `["band", pool, direction, band_id_le]`
    /// 10. `[]`        System program
    /// 11. `[]`        Token program for mint A
    /// 12. `[]`        Token program for mint B
    OpenLoan {
        sides: u8,
        collateral_amount: u64,
        debt_amount: u64,
        nonce: u64,
    },

    /// Repay a loan in full (no partial repay in v1). Transfers the
    /// principal-plus-accrued debt back into the pool, releases the
    /// collateral to the borrower, decrements the band's membership `count`
    /// (refunding the band's rent if it empties), and marks the `Loan` as
    /// repaid.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool
    /// 1. `[writable]` Vault A
    /// 2. `[writable]` Vault B
    /// 3. `[writable]` Borrower's token account A
    /// 4. `[writable]` Borrower's token account B
    /// 5. `[]`         Mint A
    /// 6. `[]`         Mint B
    /// 7. `[writable, signer]` Borrower
    /// 8. `[writable]` Loan
    /// 9. `[writable]` Band
    /// 10. `[]`        Token program for mint A
    /// 11. `[]`        Token program for mint B
    RepayLoan,

    /// Drain accumulated protocol fees from the vaults to the caller's token
    /// accounts. Resets `protocol_fees_a` / `protocol_fees_b` to 0; no-op if
    /// both are already 0.
    ///
    /// Gated on the **program's upgrade authority** (not a pool authority —
    /// pools have none): the signer must equal the `upgrade_authority_address`
    /// recorded in the program's ProgramData account. This ties fee redemption
    /// to whoever controls the program, and follows automatically if the
    /// upgrade authority is transferred.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool
    /// 1. `[writable]` Vault A
    /// 2. `[writable]` Vault B
    /// 3. `[writable]` Recipient token A account
    /// 4. `[writable]` Recipient token B account
    /// 5. `[]`         Mint A
    /// 6. `[]`         Mint B
    /// 7. `[signer]`   Program upgrade authority (fee recipient)
    /// 8. `[]`         Program ProgramData account (source of the upgrade authority)
    /// 9. `[]`         Token program for mint A
    /// 10. `[]`        Token program for mint B
    ClaimProtocolFees,

    /// Borrower-callable: reclaim the rent on a `Loan` that was liquidated
    /// mid-swap.
    ///
    /// Accounts:
    /// 0. `[writable]` Loan (status=LIQUIDATED, drained + zeroed)
    /// 1. `[writable, signer]` Borrower (lamport recipient; must match
    ///    `Loan.borrower`)
    ClaimLiquidatedRent,

    /// Swap with mandatory in-flight liquidation. See DESIGN.md §7.
    ///
    /// Caller supplies all bands+loans whose liquidation might be triggered by
    /// the price move. The program iteratively (a) computes the post-swap
    /// price, (b) finds the next supplied loan whose direction matches and
    /// whose trigger has been crossed, (c) liquidates it, and (d) recomputes.
    /// After the loop terminates, the swap is quoted on the final accounted
    /// reserves and committed only if it satisfies the user's `min_out` and the
    /// pool's executable cap.
    ///
    /// `band_loan_counts[i]` = number of loans supplied for the `i`-th band
    /// (must equal that band's `count`). The total tail account count is
    /// `Σ (1 + band_loan_counts[i])`.
    ///
    /// Accounts (fixed prefix):
    /// 0. `[writable]` Pool
    /// 1. `[writable]` Vault A
    /// 2. `[writable]` Vault B
    /// 3. `[writable]` User token A
    /// 4. `[writable]` User token B
    /// 5. `[]`         Mint A
    /// 6. `[]`         Mint B
    /// 7. `[signer]`   User
    /// 8. `[]`         Token program for mint A
    /// 9. `[]`         Token program for mint B
    ///
    /// Accounts (per band, repeated for each entry in `band_loan_counts`):
    ///   `[writable]` Band PDA
    ///   `[writable]` Loan × K (all of the band's open loans, sorted strictly
    ///                ascending by pubkey)
    Swap {
        amount_in: u64,
        min_out: u64,
        a_to_b: bool,
        /// Band-completeness boundary, interpreted by direction:
        /// - `a_to_b` (price falls → OnFall): caller asserts the post-swap
        ///   price's band id is `≥ band_boundary`. Program verifies every
        ///   populated band with `band_id ≥ band_boundary` (per the pool's
        ///   `band_bitmap_fall`) is included in the supplied tail.
        /// - `!a_to_b` (price rises → OnRise): caller asserts the post-swap
        ///   price's band id is `≤ band_boundary`. Program verifies the
        ///   analogous condition against `band_bitmap_rise`.
        band_boundary: u32,
        band_loan_counts: Vec<u8>,
    },
}

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

#[cfg(not(feature = "no-entrypoint"))]
use solana_security_txt::security_txt;

#[cfg(not(feature = "no-entrypoint"))]
security_txt! {
    name: "ChiefLiquidity",
    project_url: "https://github.com/TibaneLabs/chiefliquidity",
    contacts: "link:https://github.com/TibaneLabs/chiefliquidity/security/advisories",
    policy: "https://github.com/TibaneLabs/chiefliquidity/security/policy",
    source_code: "https://github.com/TibaneLabs/chiefliquidity"
}

/// Program entrypoint.
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    if program_id != &crate::id() {
        return Err(ProgramError::IncorrectProgramId);
    }

    let instruction = LiquidityInstruction::try_from_slice(instruction_data)
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    match instruction {
        LiquidityInstruction::InitializePool => {
            msg!("Instruction: InitializePool");
            process_initialize_pool(program_id, accounts)
        }
        LiquidityInstruction::AddLiquidity {
            amount_a_max,
            amount_b_max,
            min_lp_out,
        } => {
            msg!("Instruction: AddLiquidity");
            process_add_liquidity(
                program_id,
                accounts,
                amount_a_max,
                amount_b_max,
                min_lp_out,
            )
        }
        LiquidityInstruction::RemoveLiquidity {
            lp_amount,
            min_a_out,
            min_b_out,
        } => {
            msg!("Instruction: RemoveLiquidity");
            process_remove_liquidity(
                program_id,
                accounts,
                lp_amount,
                min_a_out,
                min_b_out,
            )
        }
        LiquidityInstruction::OpenLoan {
            sides,
            collateral_amount,
            debt_amount,
            nonce,
        } => {
            msg!("Instruction: OpenLoan");
            process_open_loan(
                program_id,
                accounts,
                sides,
                collateral_amount,
                debt_amount,
                nonce,
            )
        }
        LiquidityInstruction::RepayLoan => {
            msg!("Instruction: RepayLoan");
            process_repay_loan(program_id, accounts)
        }
        LiquidityInstruction::ClaimProtocolFees => {
            msg!("Instruction: ClaimProtocolFees");
            process_claim_protocol_fees(program_id, accounts)
        }
        LiquidityInstruction::ClaimLiquidatedRent => {
            msg!("Instruction: ClaimLiquidatedRent");
            process_claim_liquidated_rent(program_id, accounts)
        }
        LiquidityInstruction::Swap {
            amount_in,
            min_out,
            a_to_b,
            band_boundary,
            band_loan_counts,
        } => {
            msg!("Instruction: Swap");
            process_swap(
                program_id,
                accounts,
                amount_in,
                min_out,
                a_to_b,
                band_boundary,
                band_loan_counts,
            )
        }
    }
}
