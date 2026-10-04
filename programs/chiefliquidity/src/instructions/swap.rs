//! Swap with mandatory in-flight liquidation. See `DESIGN.md` §7.
//!
//! High-level flow:
//!   1. Load pool, vault balances, accounted/swappable reserves. On the first
//!      swap of a slot, fold the pre-swap spot price into the reference-price
//!      EMA (§12).
//!   2. Parse the variable account tail into per-band `(band, [loans])` and
//!      prove each supplied band's membership is complete (§6.5): exactly
//!      `band.count` distinct open loans, sorted ascending by pubkey, each with
//!      matching `band_id` and `direction`.
//!   3. Iteratively: compute post-swap price (the accounted price after
//!      settlement: input side + amount_in − protocol skim, output side −
//!      amount_out) → find next supplied loan that's triggered (direction
//!      matches, trigger crosses post_price) → liquidate → recompute. Cap at
//!      `MAX_LIQ_PER_SWAP`.
//!   4. Final swap quote against the post-liquidation accounted reserves
//!      (the loop's last quote); zero output reverts.
//!   5. Enforce slippage gate and executable-reserve cap.
//!   6. Move tokens, persist updated accounts.
//!
//! Liquidation is purely accounting: collateral was already in the vault,
//! debt tokens are simply forgiven. No SPL transfers happen during the
//! liquidation phase; only the final swap moves tokens.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::AccountInfo,
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    pubkey::Pubkey,
    sysvar::Sysvar,
};
use spl_token_2022::{
    extension::StateWithExtensions,
    state::{Account as TokenAccount, Mint},
};

use crate::{
    error::LiquidityError,
    events::{Event, LoanLiquidated, SwapExecuted},
    math::{
        band_id_for_trigger, cpmm_quote_out, is_liquidatable, price_b_per_a_wad, LoanSides,
        TriggerDirection, BPS_DENOM,
    },
    state::{
        bitmap_clear, bitmap_iter_set_range, validate_token_program_for_mint, Loan, LoanIndexBand,
        Pool, BAND_SEED, POOL_SEED,
    },
};

/// Per-transaction cap on liquidations, to bound CU and account-list usage.
pub const MAX_LIQ_PER_SWAP: u32 = 8;

const FIXED_PREFIX_LEN: usize = 10;

#[derive(Debug)]
struct LoanCtx {
    /// Index into `accounts` for this loan's `Loan`.
    loan_idx: usize,
    /// Trigger price (denormalized for fast comparison).
    trigger_wad: u128,
    /// Trigger direction (denormalized).
    direction: TriggerDirection,
    /// Loan sides (denormalized).
    sides: LoanSides,
    /// Original collateral amount.
    collateral: u128,
    /// Original debt principal (accrued is bonus to LP).
    debt_principal: u128,
    /// Has this loan been liquidated in this swap?
    liquidated: bool,
}

#[derive(Debug)]
struct BandCtx {
    /// Index into `accounts` for the band PDA.
    band_idx: usize,
    /// Indices (into `accounts`) of this band's supplied loans.
    loan_idxs: Vec<usize>,
}

#[allow(clippy::too_many_arguments)]
pub fn process_swap(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount_in: u64,
    min_out: u64,
    a_to_b: bool,
    band_boundary: u32,
    band_loan_counts: Vec<u8>,
) -> ProgramResult {
    if accounts.len() < FIXED_PREFIX_LEN {
        return Err(LiquidityError::InvalidLiquidationContext.into());
    }
    let pool_info = &accounts[0];
    let vault_a_info = &accounts[1];
    let vault_b_info = &accounts[2];
    let user_a_info = &accounts[3];
    let user_b_info = &accounts[4];
    let mint_a_info = &accounts[5];
    let mint_b_info = &accounts[6];
    let user_info = &accounts[7];
    let token_program_a_info = &accounts[8];
    let token_program_b_info = &accounts[9];

    if !user_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    validate_token_program_for_mint(token_program_a_info, mint_a_info)?;
    validate_token_program_for_mint(token_program_b_info, mint_b_info)?;
    if pool_info.owner != program_id {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }
    if amount_in == 0 {
        return Err(LiquidityError::ZeroAmount.into());
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

    // ---- Parse the variable tail ----
    // Layout per band: [Band PDA, Loan × k]. The caller proves it supplied a
    // band's full membership (DESIGN.md §6.5) by handing over exactly
    // `band.count` distinct open loans, sorted strictly ascending by pubkey,
    // each carrying this band's `band_id` and the swap's trigger direction.
    // `k` distinct members of a band whose true size is `band.count` are, by
    // pigeonhole, exactly the band — so nothing triggerable can be omitted.
    let mut bands: Vec<BandCtx> = Vec::with_capacity(band_loan_counts.len());
    let mut loans: Vec<LoanCtx> = Vec::new();
    let mut supplied_band_ids: Vec<u32> = Vec::with_capacity(band_loan_counts.len());
    let mut cursor = FIXED_PREFIX_LEN;
    // a_to_b = user deposits A, withdraws B → vault A grows, vault B shrinks
    //          → price_b_per_a = accounted_b / accounted_a FALLS
    //          → CollateralA / OnFall loans become liquidatable.
    // b_to_a is the mirror.
    let expected_direction = if a_to_b {
        TriggerDirection::OnFall
    } else {
        TriggerDirection::OnRise
    };
    let expected_dir_byte = expected_direction as u8;

    for &k_u8 in band_loan_counts.iter() {
        let k = k_u8 as usize;
        let needed = 1 + k;
        if cursor + needed > accounts.len() {
            return Err(LiquidityError::InvalidLiquidationContext.into());
        }
        let band_idx = cursor;
        let band_info = &accounts[band_idx];
        if band_info.owner != program_id {
            return Err(LiquidityError::InvalidAccountOwner.into());
        }
        let band = LoanIndexBand::try_from_slice(&band_info.try_borrow_data()?)
            .map_err(|_| LiquidityError::AccountDataTooSmall)?;
        if !band.is_initialized()
            || band.pool != *pool_info.key
            || band.direction != expected_dir_byte
        {
            return Err(LiquidityError::BandMismatch.into());
        }
        // Defense in depth: the account must also sit at the band PDA its
        // contents claim (stored bump ⇒ one cheap hash, no bump search).
        let expected_band = Pubkey::create_program_address(
            &[
                BAND_SEED,
                pool_info.key.as_ref(),
                &[band.direction],
                &band.band_id.to_le_bytes(),
                &[band.bump],
            ],
            program_id,
        )
        .map_err(|_| LiquidityError::BandMismatch)?;
        if *band_info.key != expected_band {
            return Err(LiquidityError::BandMismatch.into());
        }
        // No band may be supplied twice — combined with strict ascension within
        // a band, this makes every supplied loan globally distinct.
        if supplied_band_ids.contains(&band.band_id) {
            return Err(LiquidityError::InvalidLiquidationContext.into());
        }
        if band.count as usize != k {
            // Caller must supply ALL loans in any band on the path.
            return Err(LiquidityError::IncompleteBandWalk.into());
        }

        let loan_start = cursor + 1;
        let mut prev_key = Pubkey::default();
        let mut loan_idxs = Vec::with_capacity(k);
        for i in 0..k {
            let loan_idx = loan_start + i;
            let loan_info = &accounts[loan_idx];
            if loan_info.owner != program_id {
                return Err(LiquidityError::InvalidAccountOwner.into());
            }
            // Strict ascension ⇒ supplied loans within this band are distinct.
            if *loan_info.key <= prev_key {
                return Err(LiquidityError::InvalidLiquidationContext.into());
            }
            prev_key = *loan_info.key;

            let loan = Loan::try_from_slice(&loan_info.try_borrow_data()?)
                .map_err(|_| LiquidityError::AccountDataTooSmall)?;
            if !loan.is_initialized() || loan.pool != *pool_info.key {
                return Err(LiquidityError::InvalidPool.into());
            }
            if !loan.is_open() {
                return Err(LiquidityError::LoanNotOpen.into());
            }
            // Membership: a loan belongs to this band iff its cached band_id and
            // trigger direction match. (band_id is immutable after open.)
            if loan.band_id != band.band_id || loan.trigger_direction != expected_dir_byte {
                return Err(LiquidityError::InvalidLiquidationContext.into());
            }
            let direction = TriggerDirection::from_u8(loan.trigger_direction)?;
            let sides = LoanSides::from_u8(loan.sides)?;
            loan_idxs.push(loan_idx);
            loans.push(LoanCtx {
                loan_idx,
                trigger_wad: loan.trigger_price_wad,
                direction,
                sides,
                collateral: loan.collateral_amount,
                debt_principal: loan.debt_principal,
                liquidated: false,
            });
        }
        supplied_band_ids.push(band.band_id);
        bands.push(BandCtx { band_idx, loan_idxs });
        cursor += needed;
    }
    if cursor != accounts.len() {
        return Err(LiquidityError::InvalidLiquidationContext.into());
    }

    // ---- Strict completeness check against pool bitmap ----
    // Every set bit in the relevant bitmap, on the relevant side of
    // `band_boundary`, must correspond to a supplied band. This catches
    // callers omitting a populated band that could have triggered loans.
    {
        let bitmap = pool.band_bitmap(expected_dir_byte)?;
        let (lo, hi) = if a_to_b {
            // OnFall: relevant bands have band_id ≥ band_id_for_trigger(post_price);
            // caller asserts post-swap band ≥ band_boundary (i.e. we crossed
            // DOWN from high band ids), so range to validate is
            // [band_boundary, MAX].
            (band_boundary, u32::MAX)
        } else {
            // OnRise: post-swap band ≤ band_boundary, range is [0, band_boundary].
            (0u32, band_boundary)
        };
        for required_id in bitmap_iter_set_range(bitmap, lo, hi) {
            if !supplied_band_ids.contains(&required_id) {
                msg!(
                    "completeness: band_id={} (dir={}) is populated but not supplied",
                    required_id,
                    expected_dir_byte
                );
                return Err(LiquidityError::IncompleteBandWalk.into());
            }
        }
    }

    // ---- Initial reserve state ----
    let mint_a_decimals = read_mint_decimals(mint_a_info)?;
    let mint_b_decimals = read_mint_decimals(mint_b_info)?;
    let real_a = read_token_amount(vault_a_info)?;
    let real_b = read_token_amount(vault_b_info)?;
    // Bump per-side borrow indexes once at swap entry. Liquidations
    // happening below remove principal from total_debt (the index isn't
    // touched again — by definition liquidations forfeit accrued interest
    // since the debt is written off, not paid back).
    let clock = Clock::get()?;
    pool.bump_indexes(real_a, real_b, clock.slot)?;
    let (mut accounted_a, mut accounted_b) = pool.accounted(real_a, real_b)?;
    if accounted_a == 0 || accounted_b == 0 {
        return Err(LiquidityError::ZeroReserves.into());
    }

    // ---- Reference price sample (DESIGN.md §12) ----
    // The first swap of each slot folds the pre-swap spot price — i.e. the
    // previous slot's close, before this swap or its liquidations move it —
    // into the EMA. Later swaps in the same slot don't sample, so a pump
    // inside a transaction can never feed the reference `OpenLoan` checks.
    if pool.ref_price_wad == 0 || clock.slot != pool.ref_price_slot {
        let spot_wad = price_b_per_a_wad(accounted_a, accounted_b)?;
        pool.ref_price_wad = pool.ref_price_at(spot_wad, clock.slot)?;
        pool.ref_price_slot = clock.slot;
    }

    // ---- Protocol fee skim ----
    // Total fee charged on the input side:
    //   fee_taken = amount_in * swap_fee_bps / BPS_DENOM
    // Protocol's share (rest stays as LP yield via implicit accounted growth):
    //   protocol_portion = fee_taken * protocol_fee_bps / swap_fee_bps
    if pool.swap_fee_bps as u128 >= BPS_DENOM {
        return Err(LiquidityError::SettingExceedsMaximum.into());
    }
    let protocol_portion: u64 = if pool.swap_fee_bps == 0 || pool.protocol_fee_bps == 0 {
        0
    } else {
        let fee_taken = (amount_in as u128) * (pool.swap_fee_bps as u128) / BPS_DENOM;
        let portion = fee_taken * (pool.protocol_fee_bps as u128) / (pool.swap_fee_bps as u128);
        portion
            .try_into()
            .map_err(|_| LiquidityError::MathOverflow)?
    };
    // What the accounted input reserve actually grows by once the swap
    // settles: the whole input minus the protocol's skim (the LP fee stays in
    // reserves). Trigger and boundary checks price against this, not against
    // the fee-reduced amount the CPMM quote uses.
    let in_growth = (amount_in - protocol_portion) as u128;

    // ---- Iterative liquidation loop ----
    // Settles on the final quote and the true post-swap price: the loop only
    // exits once no supplied, unliquidated loan triggers at that price.
    let mut liq_count = 0u32;
    let (amount_out, final_post_price_wad) = loop {
        // Quote and compute the would-be post_price.
        let (in_reserve, out_reserve) = if a_to_b {
            (accounted_a, accounted_b)
        } else {
            (accounted_b, accounted_a)
        };
        if in_reserve == 0 || out_reserve == 0 {
            return Err(LiquidityError::ZeroReserves.into());
        }
        let amount_out = cpmm_quote_out(
            amount_in as u128,
            in_reserve,
            out_reserve,
            pool.swap_fee_bps,
        )?;
        let post_price_wad =
            post_swap_price_wad(accounted_a, accounted_b, in_growth, amount_out, a_to_b)?;

        // Find next triggered, not-yet-liquidated loan.
        let next_idx = loans.iter().position(|l| {
            !l.liquidated && is_liquidatable(l.trigger_wad, l.direction, post_price_wad)
        });
        let i = match next_idx {
            Some(i) => i,
            None => break (amount_out, post_price_wad),
        };

        if liq_count >= MAX_LIQ_PER_SWAP {
            return Err(LiquidityError::TooManyLiquidationsRequired.into());
        }

        // Apply liquidation (accounting only — collateral already in vault,
        // debt tokens were already paid out).
        let lc = &mut loans[i];
        match lc.sides {
            LoanSides::CollateralA => {
                pool.total_collateral_a = pool
                    .total_collateral_a
                    .checked_sub(lc.collateral)
                    .ok_or(LiquidityError::MathUnderflow)?;
                pool.total_debt_b = pool
                    .total_debt_b
                    .checked_sub(lc.debt_principal)
                    .ok_or(LiquidityError::MathUnderflow)?;
            }
            LoanSides::CollateralB => {
                pool.total_collateral_b = pool
                    .total_collateral_b
                    .checked_sub(lc.collateral)
                    .ok_or(LiquidityError::MathUnderflow)?;
                pool.total_debt_a = pool
                    .total_debt_a
                    .checked_sub(lc.debt_principal)
                    .ok_or(LiquidityError::MathUnderflow)?;
            }
        }
        pool.open_loans = pool
            .open_loans
            .checked_sub(1)
            .ok_or(LiquidityError::MathUnderflow)?;
        lc.liquidated = true;
        liq_count += 1;

        // Recompute accounted reserves with the new pool totals.
        let (new_a, new_b) = pool.accounted(real_a, real_b)?;
        accounted_a = new_a;
        accounted_b = new_b;
    };

    // ---- Final quote (post-liquidation accounted reserves, from the loop) ----
    // Never take input for nothing: dust swaps that round to zero output
    // revert even with `min_out == 0`.
    if amount_out == 0 {
        return Err(LiquidityError::ZeroAmount.into());
    }
    let amount_out_u64: u64 = amount_out
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;

    if amount_out_u64 < min_out {
        return Err(LiquidityError::SlippageExceeded.into());
    }

    // Executable cap: the output reserve's swappable portion must cover us.
    let (swappable_a, swappable_b) = pool.swappable(real_a, real_b)?;
    let out_swappable = if a_to_b { swappable_b } else { swappable_a };
    if amount_out > out_swappable {
        return Err(LiquidityError::Insolvent.into());
    }

    // ---- Accrue the protocol fee skim (computed above) ----
    if protocol_portion > 0 {
        if a_to_b {
            pool.protocol_fees_a = pool
                .protocol_fees_a
                .checked_add(protocol_portion)
                .ok_or(LiquidityError::MathOverflow)?;
        } else {
            pool.protocol_fees_b = pool
                .protocol_fees_b
                .checked_add(protocol_portion)
                .ok_or(LiquidityError::MathOverflow)?;
        }
    }

    // ---- Final boundary check: post-swap price's band must be on the
    // claimed side of `band_boundary`. If the cascade pushed the price past
    // the caller's claim, more bands could have triggered. Revert. The price
    // is the loop's final one — the same the trigger checks used — and equals
    // the accounted price the pool will actually show after settlement. ----
    let final_post_band = band_id_for_trigger(final_post_price_wad)?;
    // For OnFall (a_to_b): final_post_band must be ≥ band_boundary (price
    // didn't fall further than claimed). For OnRise (b_to_a): ≤ band_boundary.
    if a_to_b {
        if final_post_band < band_boundary {
            return Err(LiquidityError::IncompleteBandWalk.into());
        }
    } else if final_post_band > band_boundary {
        return Err(LiquidityError::IncompleteBandWalk.into());
    }

    // ---- Persist liquidated loans + rewire band chains ----
    persist_liquidations(accounts, &loans, &bands, &mut pool, expected_dir_byte)?;

    // ---- Token transfers ----
    let pool_pda_seeds: &[&[u8]] = &[
        POOL_SEED,
        pool.mint_a.as_ref(),
        pool.mint_b.as_ref(),
        std::slice::from_ref(&pool.pool_bump),
    ];
    if a_to_b {
        // user A → vault A
        invoke(
            &spl_token_2022::instruction::transfer_checked(
                token_program_a_info.key,
                user_a_info.key,
                mint_a_info.key,
                vault_a_info.key,
                user_info.key,
                &[],
                amount_in,
                mint_a_decimals,
            )?,
            &[
                user_a_info.clone(),
                mint_a_info.clone(),
                vault_a_info.clone(),
                user_info.clone(),
            ],
        )?;
        // vault B → user B
        invoke_signed(
            &spl_token_2022::instruction::transfer_checked(
                token_program_b_info.key,
                vault_b_info.key,
                mint_b_info.key,
                user_b_info.key,
                pool_info.key,
                &[],
                amount_out_u64,
                mint_b_decimals,
            )?,
            &[
                vault_b_info.clone(),
                mint_b_info.clone(),
                user_b_info.clone(),
                pool_info.clone(),
            ],
            &[pool_pda_seeds],
        )?;
    } else {
        // user B → vault B
        invoke(
            &spl_token_2022::instruction::transfer_checked(
                token_program_b_info.key,
                user_b_info.key,
                mint_b_info.key,
                vault_b_info.key,
                user_info.key,
                &[],
                amount_in,
                mint_b_decimals,
            )?,
            &[
                user_b_info.clone(),
                mint_b_info.clone(),
                vault_b_info.clone(),
                user_info.clone(),
            ],
        )?;
        // vault A → user A
        invoke_signed(
            &spl_token_2022::instruction::transfer_checked(
                token_program_a_info.key,
                vault_a_info.key,
                mint_a_info.key,
                user_a_info.key,
                pool_info.key,
                &[],
                amount_out_u64,
                mint_a_decimals,
            )?,
            &[
                vault_a_info.clone(),
                mint_a_info.clone(),
                user_a_info.clone(),
                pool_info.clone(),
            ],
            &[pool_pda_seeds],
        )?;
    }

    pool.last_update_slot = clock.slot;
    let mut data = pool_info.try_borrow_mut_data()?;
    pool.serialize(&mut &mut data[..])?;

    msg!(
        "Swap a_to_b={} amount_in={} amount_out={} liquidations={}",
        a_to_b,
        amount_in,
        amount_out_u64,
        liq_count
    );
    SwapExecuted {
        pool: *pool_info.key,
        user: *user_info.key,
        a_to_b,
        amount_in,
        amount_out: amount_out_u64,
        liquidations: liq_count,
        protocol_fee: protocol_portion,
    }
    .emit();
    Ok(())
}

/// B-per-A price of the accounted reserves once a swap settles: the input
/// side grows by `in_growth` (amount in minus the protocol skim), the output
/// side shrinks by `amount_out`.
fn post_swap_price_wad(
    accounted_a: u128,
    accounted_b: u128,
    in_growth: u128,
    amount_out: u128,
    a_to_b: bool,
) -> Result<u128, LiquidityError> {
    let (post_a, post_b) = if a_to_b {
        (
            accounted_a
                .checked_add(in_growth)
                .ok_or(LiquidityError::MathOverflow)?,
            accounted_b
                .checked_sub(amount_out)
                .ok_or(LiquidityError::MathUnderflow)?,
        )
    } else {
        (
            accounted_a
                .checked_sub(amount_out)
                .ok_or(LiquidityError::MathUnderflow)?,
            accounted_b
                .checked_add(in_growth)
                .ok_or(LiquidityError::MathOverflow)?,
        )
    };
    // price_b_per_a_wad rejects post_a == 0 with ZeroReserves.
    price_b_per_a_wad(post_a, post_b)
}

/// For each band, tombstone its liquidated loans and decrement the band's
/// membership `count` accordingly. Liquidated loans are marked
/// `STATUS_LIQUIDATED` with their amounts zeroed (the tombstone preserves
/// off-chain auditability; lamports remain recoverable by the borrower via
/// `ClaimLiquidatedRent`). Survivors need no rewiring — band membership is
/// just a count, not an ordered chain.
///
/// If a band's `count` drops to 0, its bit in the pool's bitmap is cleared so
/// subsequent swaps don't have to supply it. The band PDA is left allocated
/// (rent recoverable later) — the bitmap is the source of truth for
/// "populated".
fn persist_liquidations(
    accounts: &[AccountInfo],
    loans: &[LoanCtx],
    bands: &[BandCtx],
    pool: &mut Pool,
    direction_byte: u8,
) -> ProgramResult {
    let clock = Clock::get()?;

    for band in bands {
        let band_info = &accounts[band.band_idx];
        let mut band_state =
            LoanIndexBand::try_from_slice(&band_info.try_borrow_data()?)
                .map_err(|_| LiquidityError::AccountDataTooSmall)?;

        let mut liquidated_here: u32 = 0;

        for &loan_idx in band.loan_idxs.iter() {
            // Each supplied loan maps 1:1 to a LoanCtx by its account index.
            let lc = loans
                .iter()
                .find(|l| l.loan_idx == loan_idx)
                .ok_or(LiquidityError::InvalidLiquidationContext)?;
            if !lc.liquidated {
                continue;
            }
            let loan_info = &accounts[loan_idx];
            // Mark Loan as liquidated with zeroed amounts. Lamports remain in
            // the account; borrower can reclaim later via ClaimLiquidatedRent.
            let mut loan = Loan::try_from_slice(&loan_info.try_borrow_data()?)
                .map_err(|_| LiquidityError::AccountDataTooSmall)?;
            // Capture pre-zero values for the event.
            LoanLiquidated {
                pool: loan.pool,
                loan: *loan_info.key,
                borrower: loan.borrower,
                sides: lc.sides as u8,
                collateral_amount: lc.collateral,
                debt_principal: lc.debt_principal,
                trigger_price_wad: lc.trigger_wad,
            }
            .emit();
            loan.collateral_amount = 0;
            loan.debt_principal = 0;
            loan.status = Loan::STATUS_LIQUIDATED;
            loan.closed_slot = clock.slot;
            let mut data = loan_info.try_borrow_mut_data()?;
            loan.serialize(&mut &mut data[..])?;
            liquidated_here += 1;
        }

        if liquidated_here == 0 {
            continue;
        }
        band_state.count = band_state
            .count
            .checked_sub(liquidated_here)
            .ok_or(LiquidityError::MathUnderflow)?;
        // Maintain bitmap invariant: bit set ↔ band has loans.
        if band_state.count == 0 {
            bitmap_clear(pool.band_bitmap_mut(direction_byte)?, band_state.band_id)?;
        }
        let mut data = band_info.try_borrow_mut_data()?;
        band_state.serialize(&mut &mut data[..])?;
    }
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

fn read_token_amount(info: &AccountInfo) -> Result<u128, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&data)
        .map_err(|_| LiquidityError::InvalidVault)?;
    Ok(state.base.amount as u128)
}
