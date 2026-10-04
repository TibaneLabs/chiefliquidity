//! Deposit liquidity and mint LP tokens against accounted reserves.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
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
    events::{Event, LiquidityAdded},
    math::{isqrt_u128, mul_div, U256},
    state::{validate_token_program_for_mint, Pool, POOL_SEED},
};

/// Minimum first-deposit per side. Prevents share-inflation tricks where the
/// initial depositor seeds the pool with dust to make subsequent deposits
/// round to zero LP.
pub const MIN_FIRST_DEPOSIT: u64 = 1_000_000;

/// Floor on the LP supply of a live pool (Uniswap-v2's `MINIMUM_LIQUIDITY`,
/// enforced as a floor instead of a burn so a sole LP can still fully exit).
///
/// The first deposit mints `sqrt(a·b) >= MIN_FIRST_DEPOSIT` LP (both sides
/// are `>= MIN_FIRST_DEPOSIT`), so a fresh pool always starts above it, and
/// `RemoveLiquidity` refuses any burn that would take the supply from
/// `>= MINIMUM_LIQUIDITY` to `0 < supply < MINIMUM_LIQUIDITY`. Exiting to
/// exactly 0 stays allowed (see `remove_liquidity.rs` for why that is safe).
///
/// Why it matters: share-inflation relies on making ONE LP unit worth a lot
/// (donate `D` into the vaults while the supply is tiny) so a later deposit's
/// LP rounds down by up to one unit's worth. With supply `S >= 1000` the
/// rounding loss per deposit is `< D / 1000`, so inflating it to `V` costs
/// the attacker `1000·V` of capital parked in the pool (vs `V` at `S = 1`):
/// a 1000x attack-cost multiplier, and deposits only revert when smaller than
/// `D / 1000`.
///
/// The floor alone is NOT sufficient: an LP holder can always burn LP
/// directly through the token program, bypassing `RemoveLiquidity`. The
/// load-bearing protection is therefore in `derive_deposit` — the depositor is
/// only charged `ceil(lp · accounted / supply)` per side for the LP actually
/// minted, so floor rounding of `lp` can never transfer value from the
/// depositor to existing LPs (beyond 1 base unit per side) at any supply.
pub const MINIMUM_LIQUIDITY: u64 = 1_000;

// A first deposit (>= MIN_FIRST_DEPOSIT per side) must always clear the floor.
const _: () = assert!(MINIMUM_LIQUIDITY <= MIN_FIRST_DEPOSIT);

pub fn process_add_liquidity(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount_a_max: u64,
    amount_b_max: u64,
    min_lp_out: u64,
) -> ProgramResult {
    let it = &mut accounts.iter();

    let pool_info = next_account_info(it)?;
    let vault_a_info = next_account_info(it)?;
    let vault_b_info = next_account_info(it)?;
    let lp_mint_info = next_account_info(it)?;
    let user_a_info = next_account_info(it)?;
    let user_b_info = next_account_info(it)?;
    let user_lp_info = next_account_info(it)?;
    let user_info = next_account_info(it)?;
    let mint_a_info = next_account_info(it)?;
    let mint_b_info = next_account_info(it)?;
    let token_program_a_info = next_account_info(it)?;
    let token_program_b_info = next_account_info(it)?;

    // ---- Validation ----
    if !user_info.is_signer {
        return Err(LiquidityError::MissingRequiredSigner.into());
    }
    // Per-side token programs (LP mint rides on mint A's program).
    validate_token_program_for_mint(token_program_a_info, mint_a_info)?;
    validate_token_program_for_mint(token_program_b_info, mint_b_info)?;
    if pool_info.owner != program_id {
        return Err(LiquidityError::InvalidAccountOwner.into());
    }
    if amount_a_max == 0 || amount_b_max == 0 {
        return Err(LiquidityError::ZeroAmount.into());
    }

    // Load pool
    let mut pool = {
        let data = pool_info.try_borrow_data()?;
        Pool::try_from_slice(&data).map_err(|_| LiquidityError::AccountDataTooSmall)?
    };
    if !pool.is_initialized() {
        return Err(LiquidityError::NotInitialized.into());
    }
    if pool.vault_a != *vault_a_info.key
        || pool.vault_b != *vault_b_info.key
        || pool.lp_mint != *lp_mint_info.key
        || pool.mint_a != *mint_a_info.key
        || pool.mint_b != *mint_b_info.key
    {
        return Err(LiquidityError::InvalidPool.into());
    }

    let mint_a_decimals = read_mint_decimals(mint_a_info)?;
    let mint_b_decimals = read_mint_decimals(mint_b_info)?;
    let lp_decimals = read_mint_decimals(lp_mint_info)?;
    let lp_supply = read_mint_supply(lp_mint_info)?;
    let real_a = read_token_amount(vault_a_info)?;
    let real_b = read_token_amount(vault_b_info)?;

    // Bump indexes before we read accounted reserves: deposits change
    // utilization, so we want the elapsed period accrued at the previous
    // utilization first.
    pool.bump_indexes(real_a, real_b, Clock::get()?.slot)?;

    // accounted_x = (real_x - total_collateral_x) + total_debt_x —
    // collateral is in the vault but earmarked, not part of LP claim.
    let (accounted_a, accounted_b) = pool.accounted(real_a, real_b)?;

    // ---- Deposit-amount derivation ----
    let (amount_a_in, amount_b_in, lp_to_mint) = derive_deposit(
        amount_a_max as u128,
        amount_b_max as u128,
        accounted_a,
        accounted_b,
        lp_supply,
    )?;

    // u64 bound checks
    let amount_a_in_u64: u64 = amount_a_in
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;
    let amount_b_in_u64: u64 = amount_b_in
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;
    let lp_to_mint_u64: u64 = lp_to_mint
        .try_into()
        .map_err(|_| LiquidityError::MathOverflow)?;

    if lp_to_mint_u64 < min_lp_out {
        return Err(LiquidityError::SlippageExceeded.into());
    }

    // ---- Transfer A from user → vault A ----
    invoke(
        &spl_token_2022::instruction::transfer_checked(
            token_program_a_info.key,
            user_a_info.key,
            mint_a_info.key,
            vault_a_info.key,
            user_info.key,
            &[],
            amount_a_in_u64,
            mint_a_decimals,
        )?,
        &[
            user_a_info.clone(),
            mint_a_info.clone(),
            vault_a_info.clone(),
            user_info.clone(),
        ],
    )?;

    // ---- Transfer B from user → vault B ----
    invoke(
        &spl_token_2022::instruction::transfer_checked(
            token_program_b_info.key,
            user_b_info.key,
            mint_b_info.key,
            vault_b_info.key,
            user_info.key,
            &[],
            amount_b_in_u64,
            mint_b_decimals,
        )?,
        &[
            user_b_info.clone(),
            mint_b_info.clone(),
            vault_b_info.clone(),
            user_info.clone(),
        ],
    )?;

    // ---- Mint LP to user (mint authority = pool PDA) ----
    let pool_seeds: &[&[u8]] = &[
        POOL_SEED,
        pool.mint_a.as_ref(),
        pool.mint_b.as_ref(),
        std::slice::from_ref(&pool.pool_bump),
    ];
    invoke_signed(
        &spl_token_2022::instruction::mint_to_checked(
            token_program_a_info.key,
            lp_mint_info.key,
            user_lp_info.key,
            pool_info.key,
            &[],
            lp_to_mint_u64,
            lp_decimals,
        )?,
        &[
            lp_mint_info.clone(),
            user_lp_info.clone(),
            pool_info.clone(),
        ],
        &[pool_seeds],
    )?;

    // No pool fields change on add_liquidity (LP supply lives on the mint, debt
    // totals are unaffected, real reserves live in the vaults). Touch
    // last_update_slot so off-chain indexers see activity.
    pool.last_update_slot = Clock::get()?.slot;
    let mut data = pool_info.try_borrow_mut_data()?;
    pool.serialize(&mut &mut data[..])?;

    msg!(
        "AddLiquidity a_in={} b_in={} lp_out={}",
        amount_a_in_u64,
        amount_b_in_u64,
        lp_to_mint_u64
    );
    LiquidityAdded {
        pool: *pool_info.key,
        user: *user_info.key,
        amount_a_in: amount_a_in_u64,
        amount_b_in: amount_b_in_u64,
        lp_minted: lp_to_mint_u64,
    }
    .emit();
    Ok(())
}

/// Compute `(amount_a_in, amount_b_in, lp_to_mint)` for a deposit offering
/// up to `(amount_a_max, amount_b_max)` against the pool's accounted reserves
/// and current LP supply.
///
/// Rounding always favours the pool, and a deposit is only charged for the LP
/// it actually receives: after `lp = floor(min(a/A, b/B) · S)` the charged
/// amounts are recomputed as `ceil(lp · A / S)` / `ceil(lp · B / S)` (never
/// more than the matched amounts, hence never more than the maxes). Without
/// this, a pool whose LP unit was inflated (tiny supply + vault donation)
/// would keep the whole deposit while minting `floor(..)` LP, silently gifting
/// up to one LP unit's worth to existing holders — the share-inflation theft.
fn derive_deposit(
    amount_a_max: u128,
    amount_b_max: u128,
    accounted_a: u128,
    accounted_b: u128,
    lp_supply: u128,
) -> Result<(u128, u128, u128), LiquidityError> {
    if lp_supply == 0 {
        // First deposit: take both maxes; LP = sqrt(a * b). Any balance
        // already sitting in the vaults (donations, rounding dust) is gifted
        // to this depositor — only the donor loses.
        if amount_a_max < MIN_FIRST_DEPOSIT as u128 || amount_b_max < MIN_FIRST_DEPOSIT as u128 {
            return Err(LiquidityError::ZeroAmount);
        }
        let lp = isqrt_u128(
            amount_a_max
                .checked_mul(amount_b_max)
                .ok_or(LiquidityError::MathOverflow)?,
        );
        // Implied by MIN_FIRST_DEPOSIT (sqrt >= 1e6); kept as a direct guard
        // so the supply floor can never be violated from the first mint.
        if lp < MINIMUM_LIQUIDITY as u128 {
            return Err(LiquidityError::ZeroAmount);
        }
        return Ok((amount_a_max, amount_b_max, lp));
    }

    if accounted_a == 0 || accounted_b == 0 {
        return Err(LiquidityError::ZeroReserves);
    }
    // ideal_b matched against amount_a_max
    let ideal_b = mul_div(amount_a_max, accounted_b, accounted_a)?;
    let (a_in, b_in) = if ideal_b <= amount_b_max {
        (amount_a_max, ideal_b)
    } else {
        let ideal_a = mul_div(amount_b_max, accounted_a, accounted_b)?;
        (ideal_a, amount_b_max)
    };
    if a_in == 0 || b_in == 0 {
        return Err(LiquidityError::ZeroAmount);
    }
    // LP = min(a/A, b/B) * lp_supply
    let lp_a = mul_div(a_in, lp_supply, accounted_a)?;
    let lp_b = mul_div(b_in, lp_supply, accounted_b)?;
    let lp = lp_a.min(lp_b);
    if lp == 0 {
        return Err(LiquidityError::ZeroAmount);
    }
    // Charge exactly what `lp` is worth, rounded up (pool-favouring).
    // lp <= a_in·S/A  ⇒  lp·A/S <= a_in  ⇒  ceil(lp·A/S) <= a_in (integer).
    let charge_a = mul_div_ceil(lp, accounted_a, lp_supply)?;
    let charge_b = mul_div_ceil(lp, accounted_b, lp_supply)?;
    debug_assert!(charge_a <= a_in && charge_b <= b_in);
    Ok((charge_a.min(a_in), charge_b.min(b_in), lp))
}

/// `ceil(a * b / c)` with a 256-bit intermediate.
fn mul_div_ceil(a: u128, b: u128, c: u128) -> Result<u128, LiquidityError> {
    if c == 0 {
        return Err(LiquidityError::MathOverflow);
    }
    let prod = U256::from_u128(a)
        .checked_mul(U256::from_u128(b))
        .ok_or(LiquidityError::MathOverflow)?;
    let c = U256::from_u128(c);
    let (q, r) = prod.div_mod(c);
    let q = if r.is_zero() { q } else { q + U256::from(1u8) };
    q.to_u128().ok_or(LiquidityError::MathOverflow)
}

fn read_mint_decimals(info: &AccountInfo) -> Result<u8, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state =
        StateWithExtensions::<Mint>::unpack(&data).map_err(|_| LiquidityError::InvalidPoolMint)?;
    Ok(state.base.decimals)
}

fn read_mint_supply(info: &AccountInfo) -> Result<u128, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state =
        StateWithExtensions::<Mint>::unpack(&data).map_err(|_| LiquidityError::InvalidPoolMint)?;
    Ok(state.base.supply as u128)
}

fn read_token_amount(info: &AccountInfo) -> Result<u128, LiquidityError> {
    let data = info
        .try_borrow_data()
        .map_err(|_| LiquidityError::AccountDataTooSmall)?;
    let state = StateWithExtensions::<TokenAccount>::unpack(&data)
        .map_err(|_| LiquidityError::InvalidVault)?;
    Ok(state.base.amount as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_deposit_clears_minimum_liquidity() {
        let (a, b, lp) = derive_deposit(
            MIN_FIRST_DEPOSIT as u128,
            MIN_FIRST_DEPOSIT as u128,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(
            (a, b),
            (MIN_FIRST_DEPOSIT as u128, MIN_FIRST_DEPOSIT as u128)
        );
        assert_eq!(lp, MIN_FIRST_DEPOSIT as u128);
        assert!(lp >= MINIMUM_LIQUIDITY as u128);
        assert_eq!(
            derive_deposit(MIN_FIRST_DEPOSIT as u128 - 1, u64::MAX as u128, 0, 0, 0),
            Err(LiquidityError::ZeroAmount)
        );
    }

    #[test]
    fn proportional_deposit_unchanged_when_exact() {
        // 100M/400M pool, supply 200M; deposit 50M + up to 1B B.
        let (a, b, lp) = derive_deposit(
            50_000_000,
            1_000_000_000,
            100_000_000,
            400_000_000,
            200_000_000,
        )
        .unwrap();
        assert_eq!((a, b, lp), (50_000_000, 200_000_000, 100_000_000));
    }

    #[test]
    fn inflated_single_unit_does_not_steal_from_depositor() {
        // The audit scenario: supply 1 (reachable by direct token-program
        // burns), D donated. Victim offers X = 1.99·D per side.
        let d: u128 = 100_000_000;
        let (acc_a, acc_b, supply) = (d, d, 1u128);
        let x = 199 * d / 100;
        let (a_in, b_in, lp) = derive_deposit(x, x, acc_a, acc_b, supply).unwrap();
        assert_eq!(lp, 1);
        // Charged exactly one unit's worth, not 1.99·D.
        assert_eq!((a_in, b_in), (d, d));
        // Victim's claim after the deposit equals what they paid.
        let claim_a = (acc_a + a_in) * lp / (supply + lp);
        assert_eq!(claim_a, a_in);
    }

    #[test]
    fn rounding_loss_bounded_to_one_base_unit() {
        // Skewed, awkward reserves: the charge never exceeds the minted LP's
        // value by more than 1 base unit per side, and never exceeds the maxes.
        let cases: &[(u128, u128, u128, u128, u128)] = &[
            (50_000_000, 50_000_000, 100_001_000, 1_000, 1_000),
            (
                123_456_789,
                987_654_321,
                333_333_333,
                777_777_777,
                1_234_567,
            ),
            (10_000_000, 2_000_000_000, 7, 1_000_003, 1_000),
            (u64::MAX as u128, u64::MAX as u128, 3, 5, 7),
        ];
        for &(ma, mb, acc_a, acc_b, s) in cases {
            let (a, b, lp) = derive_deposit(ma, mb, acc_a, acc_b, s).unwrap();
            assert!(a <= ma && b <= mb);
            let val_a = mul_div(lp, acc_a, s).unwrap();
            let val_b = mul_div(lp, acc_b, s).unwrap();
            assert!(a >= val_a && a - val_a <= 1, "a={a} val_a={val_a}");
            assert!(b >= val_b && b - val_b <= 1, "b={b} val_b={val_b}");
            // Existing LPs are never diluted: a/A >= lp/S and b/B >= lp/S.
            assert!(a * s >= lp * acc_a && b * s >= lp * acc_b);
        }
    }

    #[test]
    fn deposit_below_one_unit_reverts() {
        assert_eq!(
            derive_deposit(1_000, 1_000, 1_000_000_000, 1_000_000_000, 1_000),
            Err(LiquidityError::ZeroAmount)
        );
    }

    #[test]
    fn mul_div_ceil_rounds_up() {
        assert_eq!(mul_div_ceil(7, 3, 2).unwrap(), 11);
        assert_eq!(mul_div_ceil(6, 3, 2).unwrap(), 9);
        assert_eq!(mul_div_ceil(0, 3, 2).unwrap(), 0);
        assert_eq!(mul_div_ceil(u128::MAX, 1, 1).unwrap(), u128::MAX);
        assert!(mul_div_ceil(u128::MAX, 2, 1).is_err());
        assert!(mul_div_ceil(1, 1, 0).is_err());
    }
}
