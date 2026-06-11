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

// Matches target/deploy/chiefliquidity-keypair.json. Regenerate the keypair
// (and update this constant) before publishing to a public cluster.
solana_program::declare_id!("D8K39AXioKew7kLfKEjsBtW3BuDXnYqntk2z4PWxzPAW");

/// Program instructions.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub enum LiquidityInstruction {
    /// Initialize a new (mint_a, mint_b) pool. Mints must be sorted; the
    /// program enforces `mint_a < mint_b` so the pool PDA is canonical.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account (PDA: ["pool", mint_a, mint_b])
    /// 1. `[]` Mint A
    /// 2. `[]` Mint B
    /// 3. `[writable]` Vault A (PDA: ["vault_a", pool])
    /// 4. `[writable]` Vault B (PDA: ["vault_b", pool])
    /// 5. `[writable]` LP mint (PDA: ["lp_mint", pool])
    /// 6. `[writable, signer]` Authority/payer
    /// 7. `[]` System program
    /// 8. `[]` Token program
    /// 9. `[]` Rent sysvar
    InitializePool {
        swap_fee_bps: u16,
        protocol_fee_bps: u16,
        liq_ratio_bps: u16,
        max_ltv_bps: u16,
        /// Base APR (bps) at zero utilization.
        interest_base_bps_per_year: u16,
        /// Slope from zero to kink utilization (bps APR added at kink).
        interest_slope1_bps_per_year: u16,
        /// Slope from kink to 100% utilization (bps APR added over kink).
        interest_slope2_bps_per_year: u16,
        /// Kink point in bps of utilization (e.g. 8000 = 80%).
        interest_kink_bps: u16,
    },

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
    /// 10. `[]`        Token program
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
    /// 10. `[]`        Token program
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
    /// 11. `[]`        Token program
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
    /// 10. `[]`        Token program
    RepayLoan,

    /// Authority-only: drain accumulated protocol fees from the vaults to
    /// the authority's token accounts. Resets `protocol_fees_a` and
    /// `protocol_fees_b` to 0. No-op if both are already 0.
    ///
    /// Reverts if pool authority has been renounced.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool
    /// 1. `[writable]` Vault A
    /// 2. `[writable]` Vault B
    /// 3. `[writable]` Authority's token A account
    /// 4. `[writable]` Authority's token B account
    /// 5. `[]`         Mint A
    /// 6. `[]`         Mint B
    /// 7. `[signer]`   Authority
    /// 8. `[]`         Token program
    ClaimProtocolFees,

    /// Authority-only: rotate the pool authority. Set `new_authority =
    /// Pubkey::default()` to permanently renounce.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool
    /// 1. `[signer]`   Current authority
    TransferAuthority { new_authority: Pubkey },

    /// Borrower-callable: reclaim the rent on a `Loan` that was liquidated
    /// mid-swap.
    ///
    /// Accounts:
    /// 0. `[writable]` Loan (status=LIQUIDATED, drained + zeroed)
    /// 1. `[writable, signer]` Borrower (lamport recipient; must match
    ///    `Loan.borrower`)
    ClaimLiquidatedRent,

    /// Authority-only: retune fee/liquidation/LTV/interest parameters
    /// within the same bounds as `InitializePool`. Applies prospectively
    /// (existing loans' trigger prices are not recomputed). The borrow
    /// indexes are bumped at the **old** rate before the new params take
    /// effect, so accrued-but-unrealized interest is captured at the
    /// previous curve.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool
    /// 1. `[]` Vault A (read-only — needed for utilization)
    /// 2. `[]` Vault B (read-only)
    /// 3. `[signer]`   Authority
    UpdatePoolSettings {
        swap_fee_bps: u16,
        protocol_fee_bps: u16,
        liq_ratio_bps: u16,
        max_ltv_bps: u16,
        interest_base_bps_per_year: u16,
        interest_slope1_bps_per_year: u16,
        interest_slope2_bps_per_year: u16,
        interest_kink_bps: u16,
    },

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
    /// 8. `[]`         Token program
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
    project_url: "https://github.com/KarpelesLab/chiefliquidity",
    contacts: "link:https://github.com/KarpelesLab/chiefliquidity/security/advisories",
    policy: "https://github.com/KarpelesLab/chiefliquidity/security/policy",
    source_code: "https://github.com/KarpelesLab/chiefliquidity"
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
        LiquidityInstruction::InitializePool {
            swap_fee_bps,
            protocol_fee_bps,
            liq_ratio_bps,
            max_ltv_bps,
            interest_base_bps_per_year,
            interest_slope1_bps_per_year,
            interest_slope2_bps_per_year,
            interest_kink_bps,
        } => {
            msg!("Instruction: InitializePool");
            process_initialize_pool(
                program_id,
                accounts,
                swap_fee_bps,
                protocol_fee_bps,
                liq_ratio_bps,
                max_ltv_bps,
                interest_base_bps_per_year,
                interest_slope1_bps_per_year,
                interest_slope2_bps_per_year,
                interest_kink_bps,
            )
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
        LiquidityInstruction::TransferAuthority { new_authority } => {
            msg!("Instruction: TransferAuthority");
            process_transfer_authority(program_id, accounts, new_authority)
        }
        LiquidityInstruction::ClaimLiquidatedRent => {
            msg!("Instruction: ClaimLiquidatedRent");
            process_claim_liquidated_rent(program_id, accounts)
        }
        LiquidityInstruction::UpdatePoolSettings {
            swap_fee_bps,
            protocol_fee_bps,
            liq_ratio_bps,
            max_ltv_bps,
            interest_base_bps_per_year,
            interest_slope1_bps_per_year,
            interest_slope2_bps_per_year,
            interest_kink_bps,
        } => {
            msg!("Instruction: UpdatePoolSettings");
            process_update_pool_settings(
                program_id,
                accounts,
                swap_fee_bps,
                protocol_fee_bps,
                liq_ratio_bps,
                max_ltv_bps,
                interest_base_bps_per_year,
                interest_slope1_bps_per_year,
                interest_slope2_bps_per_year,
                interest_kink_bps,
            )
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
