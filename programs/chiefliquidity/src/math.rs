//! Fixed-point math for AMM quoting and liquidation-trigger derivation.
//!
//! Scale factor: 10^18 (WAD precision). 256-bit intermediates via the `uint`
//! crate, same pattern as ../chiefstaker.

use crate::error::LiquidityError;
use uint::construct_uint;

construct_uint! {
    /// 256-bit unsigned integer for large intermediate products.
    pub struct U256(4);
}

/// WAD scale factor: 10^18.
pub const WAD: u128 = 1_000_000_000_000_000_000;
pub const WAD_U256: U256 = U256([WAD as u64, (WAD >> 64) as u64, 0, 0]);

/// 100% in basis points.
pub const BPS_DENOM: u128 = 10_000;

/// Slots per year, assuming 400ms per slot. `365.25 * 86400 / 0.4`.
pub const SLOTS_PER_YEAR: u64 = 78_840_000;

impl U256 {
    pub const fn from_u128(val: u128) -> Self {
        U256([val as u64, (val >> 64) as u64, 0, 0])
    }

    pub fn to_u128(&self) -> Option<u128> {
        if self.0[2] != 0 || self.0[3] != 0 {
            return None;
        }
        Some(((self.0[1] as u128) << 64) | self.0[0] as u128)
    }
}

/// Multiply two WAD-scaled values, returning a WAD-scaled result.
pub fn wad_mul(a: u128, b: u128) -> Result<u128, LiquidityError> {
    let result = U256::from_u128(a)
        .checked_mul(U256::from_u128(b))
        .ok_or(LiquidityError::MathOverflow)?
        / WAD_U256;
    result.to_u128().ok_or(LiquidityError::MathOverflow)
}

/// Divide two WAD-scaled values, returning a WAD-scaled result.
pub fn wad_div(a: u128, b: u128) -> Result<u128, LiquidityError> {
    if b == 0 {
        return Err(LiquidityError::MathOverflow);
    }
    let result = U256::from_u128(a)
        .checked_mul(WAD_U256)
        .ok_or(LiquidityError::MathOverflow)?
        / U256::from_u128(b);
    result.to_u128().ok_or(LiquidityError::MathOverflow)
}

// ===== Integer math helpers =====

/// Integer square root by Newton's method. Returns floor(sqrt(n)).
pub fn isqrt_u128(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    // Initial estimate: 2^(ceil(log2(n)) / 2). Use a fast bit-length-based seed.
    let bits = 128 - n.leading_zeros() as u128;
    let mut x = 1u128 << bits.div_ceil(2);
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// Multiply-then-divide on u128 with U256 intermediate to avoid overflow.
/// Returns floor(a * b / c).
pub fn mul_div(a: u128, b: u128, c: u128) -> Result<u128, LiquidityError> {
    if c == 0 {
        return Err(LiquidityError::MathOverflow);
    }
    let prod = U256::from_u128(a)
        .checked_mul(U256::from_u128(b))
        .ok_or(LiquidityError::MathOverflow)?;
    let q = prod / U256::from_u128(c);
    q.to_u128().ok_or(LiquidityError::MathOverflow)
}

// ===== Interest model (utilization-based, per-side) =====
//
// We use a monotonic per-side borrow index, WAD-scaled, that grows over
// time at a rate determined by the side's utilization. Loans store the
// index snapshot at open; total owed at any future moment is
// `principal * current_index / snapshot_index`.
//
// Rate curve (Aave-style with one kink):
//   below kink: rate = base + slope1 * util / kink
//   at  kink:   rate = base + slope1
//   above kink: rate = base + slope1 + slope2 * (util - kink) / (1 - kink)
//
// All bps inputs are in "bps per year" units (10000 bps = 100% APR).
// Utilization is `total_debt / accounted`, returned as a u128 in WAD scale
// (so 0.8 utilization → 8e17).

/// Compute utilization as a WAD-scaled u128. Returns 0 when accounted is 0
/// (an empty pool has no utilization). Caps at WAD if debt exceeds accounted
/// (shouldn't happen in normal operation but a safe ceiling).
pub fn utilization_wad(debt: u128, accounted: u128) -> u128 {
    if accounted == 0 {
        return 0;
    }
    let util_u256 = U256::from_u128(debt)
        .checked_mul(WAD_U256)
        .map(|n| n / U256::from_u128(accounted))
        .unwrap_or(WAD_U256);
    util_u256.to_u128().unwrap_or(WAD).min(WAD)
}

/// Compute borrow rate in WAD-per-year units given utilization (WAD) and the
/// curve params (all in bps).
///
/// `kink_bps` is the utilization (in bps of utilization, NOT of the year)
/// at which the curve switches to the steep slope. e.g. 8000 = 80%.
pub fn compute_borrow_rate_wad_per_year(
    util_wad: u128,
    base_bps_per_year: u16,
    slope1_bps_per_year: u16,
    slope2_bps_per_year: u16,
    kink_bps: u16,
) -> Result<u128, LiquidityError> {
    let kink_wad = (kink_bps as u128)
        .checked_mul(WAD)
        .ok_or(LiquidityError::MathOverflow)?
        / BPS_DENOM;
    if kink_wad == 0 {
        return Err(LiquidityError::SettingExceedsMaximum);
    }

    let base = bps_to_wad(base_bps_per_year);
    let slope1 = bps_to_wad(slope1_bps_per_year);
    let slope2 = bps_to_wad(slope2_bps_per_year);

    if util_wad <= kink_wad {
        // base + slope1 * util / kink
        let inc = mul_div(slope1, util_wad, kink_wad)?;
        base.checked_add(inc).ok_or(LiquidityError::MathOverflow)
    } else {
        // base + slope1 + slope2 * (util - kink) / (WAD - kink)
        let above = util_wad - kink_wad;
        let span = WAD - kink_wad;
        let inc = mul_div(slope2, above, span)?;
        base.checked_add(slope1)
            .and_then(|v| v.checked_add(inc))
            .ok_or(LiquidityError::MathOverflow)
    }
}

fn bps_to_wad(bps: u16) -> u128 {
    (bps as u128) * (WAD / BPS_DENOM)
}

/// Hard ceiling for a per-side borrow index: `2^64 · WAD` (≈ 1.84e37, about
/// 1/18 of `u128::MAX`). Interest simply stops accruing once an index reaches
/// it; `bump_index_wad` saturates here instead of erroring, so the index can
/// never brick `Pool::bump_indexes` (and with it every instruction, including
/// `remove_liquidity`).
///
/// Why this value. Indexes start at `WAD` and are monotone, so a loan's
/// snapshot is always `≥ WAD` and the current index is `≤ INDEX_CAP_WAD`.
/// Hence for any `u64` principal:
///
/// ```text
///   owed = ceil(principal · current / snapshot)
///        ≤ principal · INDEX_CAP_WAD / WAD
///        ≤ (2^64 − 1) · 2^64  <  u128::MAX
/// ```
///
/// so `owed_from_index` always fits a `u128` (it may still exceed `u64`, which
/// the token-transfer layer rejects cleanly). Inside `bump_index_wad` the
/// U256 product `index · rate · slots` is at most
/// `2^124 · 2^65 · 2^64 = 2^253` even for the theoretical maximum rate
/// (`3 · u16::MAX` bps ≈ 19.7 WAD/yr) and `u64::MAX` elapsed slots, so it
/// cannot overflow either; the saturating code paths are belt-and-braces.
///
/// At the strongest configured curve (~3.04 WAD/yr at 100% utilization,
/// compounding on every bump) the cap is reached after ~14.5 years — roughly
/// a year before the uncapped index would have overflowed `u128`.
pub const INDEX_CAP_WAD: u128 = WAD << 64;

/// Linear per-slot bump: `index *= 1 + rate_per_slot * slots_elapsed`.
/// rate_per_slot = rate_per_year / SLOTS_PER_YEAR.
///
/// Returns the new index. Compounding happens between bumps (each call
/// integrates over `slots_elapsed` at constant rate); within a bump the
/// approximation is linear, which under-estimates true e^(rt) by a small
/// amount for long inactive windows. Acceptable for v1.
///
/// Infallible: the result saturates at [`INDEX_CAP_WAD`]. An index already at
/// or above the cap is returned unchanged (indexes never decrease).
pub fn bump_index_wad(
    current_index_wad: u128,
    rate_wad_per_year: u128,
    slots_elapsed: u64,
) -> u128 {
    if rate_wad_per_year == 0 || slots_elapsed == 0 || current_index_wad >= INDEX_CAP_WAD {
        return current_index_wad;
    }
    // delta = index * rate_per_year * slots / (SLOTS_PER_YEAR * WAD)
    let num = U256::from_u128(current_index_wad)
        .checked_mul(U256::from_u128(rate_wad_per_year))
        .and_then(|n| n.checked_mul(U256::from_u128(slots_elapsed as u128)));
    let Some(num) = num else {
        return INDEX_CAP_WAD;
    };
    // SLOTS_PER_YEAR * WAD ≈ 7.9e25 — fits a u128 trivially.
    let denom = U256::from_u128(SLOTS_PER_YEAR as u128 * WAD);
    let new_index = U256::from_u128(current_index_wad).saturating_add(num / denom);
    match new_index.to_u128() {
        Some(v) => v.min(INDEX_CAP_WAD),
        None => INDEX_CAP_WAD,
    }
}

/// Loan-to-value ratio in basis points, using the pool mid-price to convert
/// the debt token's value into the collateral token's units. Computed entirely
/// in 256-bit space so no intermediate product can overflow `u128` (which,
/// under `overflow-checks`, would abort the instruction rather than return a
/// clean error).
///
/// `ltv_bps = debt * BPS_DENOM * price_num / (collateral * price_den)` where
/// `(price_num, price_den)` are the accounted reserves for the conversion:
///   - CollateralA, DebtB:  `price_num = accounted_a`, `price_den = accounted_b`
///   - CollateralB, DebtA:  `price_num = accounted_b`, `price_den = accounted_a`
pub fn ltv_bps(
    debt_amount: u128,
    collateral_amount: u128,
    price_num: u128,
    price_den: u128,
) -> Result<u128, LiquidityError> {
    let denom = U256::from_u128(collateral_amount)
        .checked_mul(U256::from_u128(price_den))
        .ok_or(LiquidityError::MathOverflow)?;
    if denom.is_zero() {
        return Err(LiquidityError::MathOverflow);
    }
    let num = U256::from_u128(debt_amount)
        .checked_mul(U256::from_u128(BPS_DENOM))
        .ok_or(LiquidityError::MathOverflow)?
        .checked_mul(U256::from_u128(price_num))
        .ok_or(LiquidityError::MathOverflow)?;
    (num / denom).to_u128().ok_or(LiquidityError::MathOverflow)
}

/// Compute current owed: `ceil(principal * current_index / snapshot_index)`.
///
/// Rounds UP (in the protocol's favour): a floor here would let short or
/// small loans repay less than they actually owe. Since `current ≥ snapshot`,
/// the result is always `≥ principal`, and exactly `principal` when no
/// interest has accrued (`current == snapshot`). Computed in 256-bit space;
/// with `principal ≤ u64::MAX` and indexes bounded by [`INDEX_CAP_WAD`] the
/// result always fits a `u128`.
pub fn owed_from_index(
    principal: u128,
    snapshot_index_wad: u128,
    current_index_wad: u128,
) -> Result<u128, LiquidityError> {
    if snapshot_index_wad == 0 {
        return Err(LiquidityError::MathOverflow);
    }
    if current_index_wad < snapshot_index_wad {
        // Index can only grow; this is an invariant violation.
        return Err(LiquidityError::MathUnderflow);
    }
    let prod = U256::from_u128(principal)
        .checked_mul(U256::from_u128(current_index_wad))
        .ok_or(LiquidityError::MathOverflow)?;
    let snapshot = U256::from_u128(snapshot_index_wad);
    let (q, r) = prod.div_mod(snapshot);
    let q = if r.is_zero() { q } else { q + U256::one() };
    q.to_u128().ok_or(LiquidityError::MathOverflow)
}

// ===== AMM quoting =====

/// Constant-product AMM quote: how much of `out` token comes back when
/// depositing `amount_in` of the input token, given current reserves and a
/// swap fee in basis points.
///
/// Formula (exact integer division, no rounding favors LP):
/// ```text
///   amount_in_after_fee = amount_in * (BPS_DENOM - fee_bps) / BPS_DENOM
///   amount_out = (amount_in_after_fee * reserve_out)
///                / (reserve_in + amount_in_after_fee)
/// ```
pub fn cpmm_quote_out(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
    fee_bps: u16,
) -> Result<u128, LiquidityError> {
    if amount_in == 0 {
        return Err(LiquidityError::ZeroAmount);
    }
    if reserve_in == 0 || reserve_out == 0 {
        return Err(LiquidityError::ZeroReserves);
    }
    let fee_bps = fee_bps as u128;
    if fee_bps >= BPS_DENOM {
        return Err(LiquidityError::SettingExceedsMaximum);
    }

    let in_after_fee = U256::from_u128(amount_in)
        .checked_mul(U256::from_u128(BPS_DENOM - fee_bps))
        .ok_or(LiquidityError::MathOverflow)?
        / U256::from_u128(BPS_DENOM);

    let numerator = in_after_fee
        .checked_mul(U256::from_u128(reserve_out))
        .ok_or(LiquidityError::MathOverflow)?;
    let denominator = U256::from_u128(reserve_in)
        .checked_add(in_after_fee)
        .ok_or(LiquidityError::MathOverflow)?;
    let out = numerator / denominator;
    out.to_u128().ok_or(LiquidityError::MathOverflow)
}

/// Compute the AMM mid-price `B per A` of a pool with the given accounted
/// reserves, returned as a WAD-scaled u128.
pub fn price_b_per_a_wad(
    accounted_a: u128,
    accounted_b: u128,
) -> Result<u128, LiquidityError> {
    if accounted_a == 0 {
        return Err(LiquidityError::ZeroReserves);
    }
    // price_wad = accounted_b * WAD / accounted_a
    let num = U256::from_u128(accounted_b)
        .checked_mul(WAD_U256)
        .ok_or(LiquidityError::MathOverflow)?;
    let result = num / U256::from_u128(accounted_a);
    result.to_u128().ok_or(LiquidityError::MathOverflow)
}

// ===== Liquidation trigger (DESIGN.md §3) =====

/// Side encoding for a loan: which token is collateral, which is debt.
///
/// Stored as `u8` on the `Loan` account.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoanSides {
    /// Collateral A, debt B.
    CollateralA = 0,
    /// Collateral B, debt A.
    CollateralB = 1,
}

impl LoanSides {
    pub fn from_u8(b: u8) -> Result<Self, LiquidityError> {
        match b {
            0 => Ok(LoanSides::CollateralA),
            1 => Ok(LoanSides::CollateralB),
            _ => Err(LiquidityError::InvalidSidesEncoding),
        }
    }
}

/// Direction in which the pool's price (B-per-A) must move for a loan to
/// become liquidatable.
///
/// Stored as `u8` (`trigger_direction`) on the `Loan` account.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerDirection {
    /// Liquidation fires when pool price *falls* below `trigger_price_wad`.
    /// (A-collateral loans: A becomes worth less in B terms.)
    OnFall = 0,
    /// Liquidation fires when pool price *rises* above `trigger_price_wad`.
    /// (B-collateral loans: B becomes worth less in A terms.)
    OnRise = 1,
}

impl TriggerDirection {
    pub fn from_u8(b: u8) -> Result<Self, LiquidityError> {
        match b {
            0 => Ok(TriggerDirection::OnFall),
            1 => Ok(TriggerDirection::OnRise),
            _ => Err(LiquidityError::InvalidSidesEncoding),
        }
    }
}

/// Compute the liquidation trigger price (B-per-A, WAD-scaled) and direction
/// for a loan with the given sides, collateral amount, debt amount, and
/// liquidation ratio in basis points (e.g. 11000 = 110%).
///
/// Closed-form derivation (see DESIGN.md §3):
/// - CollateralA, DebtB: `trigger = (debt_b * liq_ratio_bps / BPS_DENOM) / collateral_a`,
///   direction = `OnFall`.
/// - CollateralB, DebtA: `trigger = collateral_b / (debt_a * liq_ratio_bps / BPS_DENOM)`,
///   direction = `OnRise`.
pub fn recompute_trigger(
    sides: LoanSides,
    collateral_amount: u128,
    debt_amount: u128,
    liq_ratio_bps: u16,
) -> Result<(u128, TriggerDirection), LiquidityError> {
    if collateral_amount == 0 {
        return Err(LiquidityError::ZeroAmount);
    }
    if debt_amount == 0 {
        return Err(LiquidityError::ZeroAmount);
    }
    let liq_ratio_bps = liq_ratio_bps as u128;

    match sides {
        LoanSides::CollateralA => {
            // trigger_wad = debt_b * liq_ratio * WAD / (collateral_a * BPS_DENOM)
            let num = U256::from_u128(debt_amount)
                .checked_mul(U256::from_u128(liq_ratio_bps))
                .ok_or(LiquidityError::MathOverflow)?
                .checked_mul(WAD_U256)
                .ok_or(LiquidityError::MathOverflow)?;
            let denom = U256::from_u128(collateral_amount)
                .checked_mul(U256::from_u128(BPS_DENOM))
                .ok_or(LiquidityError::MathOverflow)?;
            let trigger = (num / denom)
                .to_u128()
                .ok_or(LiquidityError::MathOverflow)?;
            Ok((trigger, TriggerDirection::OnFall))
        }
        LoanSides::CollateralB => {
            // trigger_wad = collateral_b * BPS_DENOM * WAD / (debt_a * liq_ratio)
            let num = U256::from_u128(collateral_amount)
                .checked_mul(U256::from_u128(BPS_DENOM))
                .ok_or(LiquidityError::MathOverflow)?
                .checked_mul(WAD_U256)
                .ok_or(LiquidityError::MathOverflow)?;
            let denom = U256::from_u128(debt_amount)
                .checked_mul(U256::from_u128(liq_ratio_bps))
                .ok_or(LiquidityError::MathOverflow)?;
            let trigger = (num / denom)
                .to_u128()
                .ok_or(LiquidityError::MathOverflow)?;
            Ok((trigger, TriggerDirection::OnRise))
        }
    }
}

/// Returns true iff a loan with the given trigger and direction is
/// liquidatable at the supplied current price.
pub fn is_liquidatable(
    trigger_price_wad: u128,
    direction: TriggerDirection,
    current_price_wad: u128,
) -> bool {
    match direction {
        TriggerDirection::OnFall => current_price_wad <= trigger_price_wad,
        TriggerDirection::OnRise => current_price_wad >= trigger_price_wad,
    }
}

// ===== Band id (DESIGN.md §6) =====

/// Bands cover 2× price ranges geometrically. `band_id` is `floor(log2(price))`
/// shifted by `BAND_OFFSET` so all in-range trigger prices land on
/// non-negative `u32` ids.
///
/// `floor(log2(WAD))` for `WAD = 10^18` is `59`. We pick `BAND_OFFSET = 64` so
/// `price = 1.0` lands at band `64`. Each step of `band_id` is a 2× change
/// in price.
pub const BAND_OFFSET: u32 = 64;
const LOG2_WAD: u32 = 59;

/// Compute `band_id` for the given trigger price (WAD-scaled).
///
/// `price = 1.0` → 64, `2.0` → 65, `0.5` → 63, etc.
/// Errors on `trigger_price_wad == 0`.
pub fn band_id_for_trigger(trigger_price_wad: u128) -> Result<u32, LiquidityError> {
    if trigger_price_wad == 0 {
        return Err(LiquidityError::ZeroAmount);
    }
    // floor(log2(x)) = 127 - leading_zeros for x: u128
    let log2_x = 127 - trigger_price_wad.leading_zeros();
    // band_id = log2(x) - log2(WAD) + BAND_OFFSET
    //         = log2_x - LOG2_WAD + BAND_OFFSET
    // log2_x is at most 127, LOG2_WAD is 59 → result fits in u32 easily.
    Ok(log2_x + BAND_OFFSET - LOG2_WAD)
}

/// Inclusive lower bound of a band's trigger-price range, WAD-scaled.
///
/// `band_id_for_trigger(band_min_wad(b)) == b` for any b in
/// `[BAND_OFFSET - LOG2_WAD, BAND_OFFSET + 67]`.
pub fn band_min_wad(band_id: u32) -> Result<u128, LiquidityError> {
    // log2_x = band_id + LOG2_WAD - BAND_OFFSET
    if band_id + LOG2_WAD < BAND_OFFSET {
        // Below the representable floor → value is 0 / 1.
        return Ok(0);
    }
    let log2_x = band_id + LOG2_WAD - BAND_OFFSET;
    if log2_x >= 128 {
        return Err(LiquidityError::MathOverflow);
    }
    Ok(1u128 << log2_x)
}

/// Exclusive upper bound of a band's trigger-price range, WAD-scaled.
pub fn band_max_wad(band_id: u32) -> Result<u128, LiquidityError> {
    band_min_wad(band_id + 1)
}

// ===== Tests =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wad_mul_basic() {
        // 1.5 * 2.0 = 3.0
        let a = 1_500_000_000_000_000_000u128;
        let b = 2_000_000_000_000_000_000u128;
        let r = wad_mul(a, b).unwrap();
        assert_eq!(r, 3_000_000_000_000_000_000u128);
    }

    #[test]
    fn test_wad_div_basic() {
        // 3.0 / 2.0 = 1.5
        let a = 3_000_000_000_000_000_000u128;
        let b = 2_000_000_000_000_000_000u128;
        let r = wad_div(a, b).unwrap();
        assert_eq!(r, 1_500_000_000_000_000_000u128);
    }

    #[test]
    fn test_wad_div_by_zero() {
        assert_eq!(wad_div(WAD, 0), Err(LiquidityError::MathOverflow));
    }

    #[test]
    fn test_cpmm_quote_no_fee() {
        // Reserves 1000/1000, deposit 100, no fee
        // amount_out = 100 * 1000 / (1000 + 100) = 100000/1100 = 90
        let out = cpmm_quote_out(100, 1000, 1000, 0).unwrap();
        assert_eq!(out, 90);
    }

    #[test]
    fn test_cpmm_quote_with_fee() {
        // 30 bps fee, so 99.7 effective
        // in_after_fee = 100 * 9970 / 10000 = 99 (truncated)
        // out = 99 * 1000 / (1000 + 99) = 99000 / 1099 = 90 (truncated)
        let out = cpmm_quote_out(100, 1000, 1000, 30).unwrap();
        assert_eq!(out, 90);
    }

    #[test]
    fn test_cpmm_quote_zero_amount() {
        assert_eq!(
            cpmm_quote_out(0, 1000, 1000, 30),
            Err(LiquidityError::ZeroAmount)
        );
    }

    #[test]
    fn test_cpmm_quote_zero_reserves() {
        assert_eq!(
            cpmm_quote_out(100, 0, 1000, 30),
            Err(LiquidityError::ZeroReserves)
        );
        assert_eq!(
            cpmm_quote_out(100, 1000, 0, 30),
            Err(LiquidityError::ZeroReserves)
        );
    }

    #[test]
    fn test_cpmm_quote_invariant_holds() {
        // x*y=k holds exactly without fee
        let r_in: u128 = 10_000;
        let r_out: u128 = 50_000;
        let amt_in: u128 = 1_000;
        let amt_out = cpmm_quote_out(amt_in, r_in, r_out, 0).unwrap();
        let new_in = r_in + amt_in;
        let new_out = r_out - amt_out;
        // CPMM rounds in favor of the LP (amt_out floored), so post-trade k >= pre-trade k.
        assert!(new_in * new_out >= r_in * r_out);
    }

    #[test]
    fn test_price_b_per_a() {
        // 1000 A and 5000 B → price = 5.0
        let p = price_b_per_a_wad(1000, 5000).unwrap();
        assert_eq!(p, 5_000_000_000_000_000_000u128);
    }

    #[test]
    fn test_recompute_trigger_collateral_a() {
        // Borrowed 100 B against 50 A, 110% liq ratio.
        // trigger = 100 * 11000 / 10000 / 50 = 110/50 = 2.2 (B per A)
        let (trigger, dir) =
            recompute_trigger(LoanSides::CollateralA, 50, 100, 11000).unwrap();
        assert_eq!(dir, TriggerDirection::OnFall);
        assert_eq!(trigger, 2_200_000_000_000_000_000u128);
    }

    #[test]
    fn test_recompute_trigger_collateral_b() {
        // Borrowed 50 A against 200 B, 110% liq ratio.
        // trigger = 200 / (50 * 1.1) = 200 / 55 ≈ 3.6363... (B per A)
        let (trigger, dir) =
            recompute_trigger(LoanSides::CollateralB, 200, 50, 11000).unwrap();
        assert_eq!(dir, TriggerDirection::OnRise);
        // 200 * 10000 * 1e18 / (50 * 11000) = 2e21 / 55e4 = 3.636363... * 1e18
        // exact integer: floor(200 * 10000 * 10^18 / 550000)
        let expected = (200u128 * 10_000 * WAD) / (50u128 * 11_000);
        assert_eq!(trigger, expected);
    }

    #[test]
    fn test_recompute_trigger_zero_amount() {
        assert_eq!(
            recompute_trigger(LoanSides::CollateralA, 0, 100, 11000),
            Err(LiquidityError::ZeroAmount)
        );
        assert_eq!(
            recompute_trigger(LoanSides::CollateralA, 100, 0, 11000),
            Err(LiquidityError::ZeroAmount)
        );
    }

    #[test]
    fn test_is_liquidatable() {
        let trig = 2_000_000_000_000_000_000u128; // 2.0
        // OnFall: liquidatable when price <= trigger
        assert!(is_liquidatable(trig, TriggerDirection::OnFall, trig));
        assert!(is_liquidatable(trig, TriggerDirection::OnFall, trig - 1));
        assert!(!is_liquidatable(trig, TriggerDirection::OnFall, trig + 1));
        // OnRise: liquidatable when price >= trigger
        assert!(is_liquidatable(trig, TriggerDirection::OnRise, trig));
        assert!(is_liquidatable(trig, TriggerDirection::OnRise, trig + 1));
        assert!(!is_liquidatable(trig, TriggerDirection::OnRise, trig - 1));
    }

    #[test]
    fn test_band_id_for_trigger() {
        // band_id = floor(log2(price_wad)) + BAND_OFFSET - LOG2_WAD.
        // floor(log2(WAD)) = 59, so price = 1.0 → 59 + 64 - 59 = 64.
        assert_eq!(band_id_for_trigger(WAD).unwrap(), 64);
        assert_eq!(band_id_for_trigger(2 * WAD).unwrap(), 65);
        assert_eq!(band_id_for_trigger(WAD / 2).unwrap(), 63);
        // Tiny price: 1 → log2 = 0 → band_id = 0 + 64 - 59 = 5
        assert_eq!(band_id_for_trigger(1).unwrap(), 5);
    }

    #[test]
    fn test_band_id_zero_errors() {
        assert_eq!(band_id_for_trigger(0), Err(LiquidityError::ZeroAmount));
    }

    #[test]
    fn test_band_min_max_consistent() {
        // For a range of band_ids, every price in [min, max) should map back.
        for b in 10u32..120 {
            let lo = band_min_wad(b).unwrap();
            let hi = band_max_wad(b).unwrap();
            if lo > 0 {
                assert_eq!(band_id_for_trigger(lo).unwrap(), b);
            }
            if hi > 1 {
                assert_eq!(band_id_for_trigger(hi - 1).unwrap(), b);
            }
            // Just-above the upper bound is in the next band
            assert_eq!(band_id_for_trigger(hi).unwrap(), b + 1);
        }
    }

    #[test]
    fn test_loan_sides_roundtrip() {
        assert_eq!(
            LoanSides::from_u8(0).unwrap(),
            LoanSides::CollateralA
        );
        assert_eq!(
            LoanSides::from_u8(1).unwrap(),
            LoanSides::CollateralB
        );
        assert_eq!(
            LoanSides::from_u8(2),
            Err(LiquidityError::InvalidSidesEncoding)
        );
    }

    #[test]
    fn test_utilization_basic() {
        assert_eq!(utilization_wad(0, 1000), 0);
        assert_eq!(utilization_wad(500, 1000), WAD / 2);
        assert_eq!(utilization_wad(1000, 1000), WAD);
        // Empty pool
        assert_eq!(utilization_wad(0, 0), 0);
        // Over-utilization (shouldn't happen, but cap at WAD)
        assert_eq!(utilization_wad(1500, 1000), WAD);
    }

    #[test]
    fn test_borrow_rate_curve() {
        // base=0, slope1=400 bps (4%), slope2=30000 bps (300%), kink=8000 (80%)
        let base = 0u16;
        let s1 = 400u16;
        let s2 = 30_000u16;
        let kink = 8000u16;

        // At 0% util → base = 0
        let r0 = compute_borrow_rate_wad_per_year(0, base, s1, s2, kink).unwrap();
        assert_eq!(r0, 0);

        // At kink (80% util) → base + slope1 = 4% APR = 0.04 WAD
        let kink_util = (kink as u128) * WAD / BPS_DENOM;
        let r_kink =
            compute_borrow_rate_wad_per_year(kink_util, base, s1, s2, kink).unwrap();
        // 4% APR in WAD = 0.04e18 = 4e16
        assert_eq!(r_kink, 40_000_000_000_000_000);

        // At 100% util → base + slope1 + slope2 = 304% APR = 3.04 WAD
        let r_full =
            compute_borrow_rate_wad_per_year(WAD, base, s1, s2, kink).unwrap();
        assert_eq!(r_full, 3_040_000_000_000_000_000);

        // At 50% util (below kink) → 0 + 400 * (50/80) = 250 bps = 2.5% APR
        let half = WAD / 2;
        let r_half = compute_borrow_rate_wad_per_year(half, base, s1, s2, kink).unwrap();
        // 0.025 WAD = 2.5e16
        assert_eq!(r_half, 25_000_000_000_000_000);
    }

    #[test]
    fn test_borrow_rate_curve_above_kink() {
        // base=0, slope1=400, slope2=30000, kink=8000
        let kink = 8000u16;
        let s1 = 400u16;
        let s2 = 30_000u16;
        // At 90% util → past kink. (90-80)/(100-80) = 0.5 of slope2 region.
        // rate = 0 + 4% + 50% * 300% = 4% + 150% = 154% APR
        let util = 9 * WAD / 10;
        let r = compute_borrow_rate_wad_per_year(util, 0, s1, s2, kink).unwrap();
        assert_eq!(r, 1_540_000_000_000_000_000);
    }

    #[test]
    fn test_bump_index_one_year_at_ten_percent() {
        // Index starts at WAD, rate = 10% APR, slots = 1 year.
        // After: index = 1.1 WAD
        let rate = WAD / 10; // 0.1 WAD/year = 10% APR
        let r = bump_index_wad(WAD, rate, SLOTS_PER_YEAR);
        assert_eq!(r, 11 * WAD / 10);
    }

    #[test]
    fn test_bump_index_zero_inputs() {
        assert_eq!(bump_index_wad(WAD, 0, 1000), WAD);
        assert_eq!(bump_index_wad(WAD, WAD / 10, 0), WAD);
    }

    /// Theoretical maximum rate: every curve parameter at `u16::MAX` bps,
    /// 100% utilization → base + slope1 + slope2 ≈ 19.66 WAD/yr.
    fn max_possible_rate() -> u128 {
        compute_borrow_rate_wad_per_year(WAD, u16::MAX, u16::MAX, u16::MAX, 1).unwrap()
    }

    #[test]
    fn test_bump_index_saturates_at_cap() {
        // One slot short of the cap with a tiny rate: lands exactly on the cap.
        let rate = WAD / 10;
        let near = INDEX_CAP_WAD - 1;
        assert_eq!(bump_index_wad(near, rate, SLOTS_PER_YEAR), INDEX_CAP_WAD);
        // At the cap: no further growth, whatever the rate / elapsed time.
        assert_eq!(bump_index_wad(INDEX_CAP_WAD, rate, 1), INDEX_CAP_WAD);
        assert_eq!(
            bump_index_wad(INDEX_CAP_WAD, max_possible_rate(), u64::MAX),
            INDEX_CAP_WAD
        );
        // Below the cap and far from it, growth is unaffected by the cap.
        let quarter = INDEX_CAP_WAD / 4;
        assert_eq!(
            bump_index_wad(quarter, rate, SLOTS_PER_YEAR),
            quarter + quarter / 10
        );
        // A (never-expected) index above the cap is left untouched, not
        // pulled down — indexes are monotone.
        assert_eq!(bump_index_wad(u128::MAX, rate, 1), u128::MAX);
    }

    #[test]
    fn test_bump_index_extreme_elapsed_does_not_error() {
        let max_rate = max_possible_rate();
        let configured_max = compute_borrow_rate_wad_per_year(WAD, 0, 400, 30_000, 8000).unwrap();
        for &rate in &[configured_max, max_rate] {
            // From a near-cap index any growth saturates at the cap.
            for &start in &[INDEX_CAP_WAD / 2, INDEX_CAP_WAD - 1] {
                assert_eq!(bump_index_wad(start, rate, u64::MAX), INDEX_CAP_WAD);
            }
            // From WAD a single linear bump (no compounding) stays well below
            // the cap even over u64::MAX slots; it must still not error.
            let r = bump_index_wad(WAD, rate, u64::MAX);
            assert!(r > WAD && r <= INDEX_CAP_WAD);
        }
    }

    #[test]
    fn test_bump_index_repeated_bumps_reach_cap_without_error() {
        // Max configured rate, bumped once per day, for 20 years: the
        // uncapped index would overflow u128 after ~15.5 years.
        let rate = compute_borrow_rate_wad_per_year(WAD, 0, 400, 30_000, 8000).unwrap();
        let per_day = SLOTS_PER_YEAR / 365;
        let mut idx = WAD;
        let mut prev = idx;
        for _ in 0..(365 * 20) {
            idx = bump_index_wad(idx, rate, per_day);
            assert!(idx >= prev && idx <= INDEX_CAP_WAD);
            prev = idx;
        }
        assert_eq!(idx, INDEX_CAP_WAD);
    }

    #[test]
    fn test_owed_at_cap_fits_u128() {
        // Worst case: max u64 principal, snapshot at WAD, index at the cap.
        let p = u64::MAX as u128;
        assert_eq!(owed_from_index(p, WAD, INDEX_CAP_WAD).unwrap(), p << 64);
    }

    #[test]
    fn test_ltv_bps_basic() {
        // CollateralA/DebtB: debt 100 B, collateral 50 A, price 5 B per A
        // (accounted_a=1000, accounted_b=5000).
        // ltv = debt * accounted_a / (collateral * accounted_b)
        //     = 100 * 1000 / (50 * 5000) = 100000 / 250000 = 0.4 → 4000 bps
        let ltv = ltv_bps(100, 50, 1000, 5000).unwrap();
        assert_eq!(ltv, 4000);
    }

    #[test]
    fn test_ltv_bps_matches_naive_formula() {
        // Cross-check against the plain-u128 formula over a range of inputs
        // that stay well inside u128 (the whole point of the U256 version is
        // to also handle the ones that don't — see the overflow test below).
        for &(debt, coll, an, bn) in &[
            (1u128, 1, 1, 1),
            (100, 50, 1000, 5000),
            (7, 3, 999, 1234),
            (1_000_000, 999_999, 123_456, 654_321),
        ] {
            let expected = ((debt * BPS_DENOM) * an) / (coll * bn);
            assert_eq!(ltv_bps(debt, coll, an, bn).unwrap(), expected);
        }
    }

    #[test]
    fn test_ltv_bps_no_overflow_at_extremes() {
        // collateral * price_den would overflow u128 in a naive computation;
        // the U256 path must instead return a real value (here ~ 1 bps region).
        let big = u128::from(u64::MAX);
        let ltv = ltv_bps(big, big, big, big).unwrap();
        // debt == collateral and price_num == price_den → ltv == BPS_DENOM.
        assert_eq!(ltv, BPS_DENOM);
    }

    #[test]
    fn test_ltv_bps_zero_denominator_errors() {
        assert_eq!(ltv_bps(100, 0, 1000, 5000), Err(LiquidityError::MathOverflow));
        assert_eq!(ltv_bps(100, 50, 1000, 0), Err(LiquidityError::MathOverflow));
    }

    #[test]
    fn test_owed_from_index() {
        // Loan opens at index=1.0 WAD with principal 100. Index grows to 1.5 WAD.
        // Owed = 100 * 1.5 / 1.0 = 150
        let principal = 100u128;
        let snap = WAD;
        let cur = 3 * WAD / 2;
        assert_eq!(owed_from_index(principal, snap, cur).unwrap(), 150);
    }

    #[test]
    fn test_owed_rounds_up() {
        // Exact division: no rounding.
        assert_eq!(owed_from_index(100, WAD, 2 * WAD).unwrap(), 200);
        assert_eq!(owed_from_index(3, 2 * WAD, 4 * WAD).unwrap(), 6);
        // Non-exact: rounds up, never down.
        assert_eq!(owed_from_index(1, WAD, 2 * WAD - 1).unwrap(), 2);
        assert_eq!(owed_from_index(1, WAD, WAD + 1).unwrap(), 2);
        assert_eq!(owed_from_index(100, WAD, WAD + 1).unwrap(), 101);
        // 13.33 → 14
        assert_eq!(owed_from_index(10, 3 * WAD, 4 * WAD).unwrap(), 14);
        // current == snapshot: exactly the principal, for any snapshot.
        for &snap in &[WAD, WAD + 1, 7 * WAD / 3, INDEX_CAP_WAD] {
            assert_eq!(owed_from_index(0, snap, snap).unwrap(), 0);
            assert_eq!(owed_from_index(12_345, snap, snap).unwrap(), 12_345);
            assert_eq!(
                owed_from_index(u64::MAX as u128, snap, snap).unwrap(),
                u64::MAX as u128
            );
        }
        // Zero principal never owes anything.
        assert_eq!(owed_from_index(0, WAD, INDEX_CAP_WAD).unwrap(), 0);
    }

    #[test]
    fn test_owed_rounds_up_large_values() {
        let p = u64::MAX as u128;
        // p * (WAD + 1) / WAD = p + p / WAD (+ fractional part → +1)
        assert_eq!(owed_from_index(p, WAD, WAD + 1).unwrap(), p + p / WAD + 1);
        // Large snapshot, exact ratio of 3/2.
        let snap = INDEX_CAP_WAD / 2;
        assert_eq!(
            owed_from_index(p - 1, snap, snap / 2 * 3).unwrap(),
            (p - 1) / 2 * 3
        );
        // Always within 1 of the floor result, and >= principal.
        for &(pr, s, c) in &[
            (p, WAD, 3 * WAD / 2 + 7),
            (
                123_456_789u128,
                1_234_567_890_123_456_789,
                9_876_543_210_987_654_321,
            ),
            (p, WAD + 12_345, INDEX_CAP_WAD - 1),
        ] {
            let floor = mul_div(pr, c, s).unwrap();
            let ceil = owed_from_index(pr, s, c).unwrap();
            assert!(ceil == floor || ceil == floor + 1);
            assert!(ceil >= pr);
        }
    }

    #[test]
    fn test_owed_rejects_index_regression() {
        // Index can never go down — assert we error rather than silently underflow.
        assert_eq!(
            owed_from_index(100, WAD, WAD - 1),
            Err(LiquidityError::MathUnderflow)
        );
    }

    #[test]
    fn test_isqrt_basic() {
        assert_eq!(isqrt_u128(0), 0);
        assert_eq!(isqrt_u128(1), 1);
        assert_eq!(isqrt_u128(2), 1);
        assert_eq!(isqrt_u128(4), 2);
        assert_eq!(isqrt_u128(9), 3);
        assert_eq!(isqrt_u128(15), 3);
        assert_eq!(isqrt_u128(16), 4);
        assert_eq!(isqrt_u128(99), 9);
        assert_eq!(isqrt_u128(100), 10);
        assert_eq!(isqrt_u128(1_000_000), 1_000);
        // Big number near u128 max
        let big = u128::MAX;
        let r = isqrt_u128(big);
        // r^2 <= big < (r+1)^2 — second part may overflow, just check r^2 <= big
        assert!(r as u128 <= u128::MAX / r as u128);
    }

    #[test]
    fn test_mul_div() {
        assert_eq!(mul_div(10, 20, 5).unwrap(), 40);
        // Big enough to overflow u128 in the intermediate without U256
        let r = mul_div(u128::MAX, 2, 4).unwrap();
        assert_eq!(r, u128::MAX / 2);
    }

    #[test]
    fn test_mul_div_zero_denom() {
        assert_eq!(mul_div(10, 20, 0), Err(LiquidityError::MathOverflow));
    }

    #[test]
    fn test_trigger_direction_roundtrip() {
        assert_eq!(
            TriggerDirection::from_u8(0).unwrap(),
            TriggerDirection::OnFall
        );
        assert_eq!(
            TriggerDirection::from_u8(1).unwrap(),
            TriggerDirection::OnRise
        );
        assert_eq!(
            TriggerDirection::from_u8(2),
            Err(LiquidityError::InvalidSidesEncoding)
        );
    }
}
