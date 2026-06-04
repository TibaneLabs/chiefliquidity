//! Structured binary log events emitted via `sol_log_data`.
//!
//! Each event is serialized as `discriminator (8 bytes) ++ borsh(payload)` and
//! emitted as a single `Program data:` log line. Off-chain consumers identify
//! events by reading the first 8 bytes of the (base64-decoded) data slice, then
//! borsh-deserialize the remainder into the matching struct.
//!
//! Discriminators are random sentinels (not Anchor-derived) and are disjoint
//! from the account discriminators in `state.rs` — events lead with `0xe_`,
//! accounts lead with `0xa_`/`0xb_`/`0xc_`/`0xd_`.
//!
//! Emitting is best-effort: a serialization failure is swallowed rather than
//! aborting the instruction (a dropped log line must never roll back a swap).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{log::sol_log_data, pubkey::Pubkey};

/// Common behavior for every event: a unique discriminator and an `emit` that
/// prefixes it before the borsh payload and writes one `sol_log_data` line.
pub trait Event: BorshSerialize {
    const DISCRIMINATOR: [u8; 8];

    fn emit(&self) {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&Self::DISCRIMINATOR);
        // Borsh serialization into a `Vec` only fails on allocation issues,
        // which would already have aborted the program. Swallow regardless:
        // a missing log line must not revert real state changes.
        if self.serialize(&mut buf).is_ok() {
            sol_log_data(&[buf.as_slice()]);
        }
    }
}

macro_rules! impl_event {
    ($t:ty, $disc:expr) => {
        impl Event for $t {
            const DISCRIMINATOR: [u8; 8] = $disc;
        }
    };
}

/// A new pool was created. Emitted by `InitializePool`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct PoolInitialized {
    pub pool: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub authority: Pubkey,
    pub swap_fee_bps: u16,
    pub protocol_fee_bps: u16,
    pub liq_ratio_bps: u16,
    pub liq_penalty_bps: u16,
    pub max_ltv_bps: u16,
    pub interest_base_bps_per_year: u16,
    pub interest_slope1_bps_per_year: u16,
    pub interest_slope2_bps_per_year: u16,
    pub interest_kink_bps: u16,
}
impl_event!(
    PoolInitialized,
    [0xe1, 0x9a, 0x4c, 0x77, 0x20, 0xb3, 0x5d, 0x08]
);

/// LP deposited liquidity and received shares. Emitted by `AddLiquidity`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct LiquidityAdded {
    pub pool: Pubkey,
    pub user: Pubkey,
    pub amount_a_in: u64,
    pub amount_b_in: u64,
    pub lp_minted: u64,
}
impl_event!(
    LiquidityAdded,
    [0xe2, 0x3f, 0x81, 0x12, 0xc6, 0x4a, 0x9e, 0x55]
);

/// LP burned shares and withdrew reserves. Emitted by `RemoveLiquidity`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct LiquidityRemoved {
    pub pool: Pubkey,
    pub user: Pubkey,
    pub lp_burned: u64,
    pub amount_a_out: u64,
    pub amount_b_out: u64,
}
impl_event!(
    LiquidityRemoved,
    [0xe3, 0x6b, 0x2d, 0xf0, 0x18, 0x97, 0x44, 0xa1]
);

/// A collateralized loan was opened. Emitted by `OpenLoan`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct LoanOpened {
    pub pool: Pubkey,
    pub loan: Pubkey,
    pub borrower: Pubkey,
    pub nonce: u64,
    /// 0 = collateral A / debt B, 1 = collateral B / debt A.
    pub sides: u8,
    pub collateral_amount: u64,
    pub debt_amount: u64,
    pub band_id: u32,
    /// 0 = TriggerOnFall, 1 = TriggerOnRise.
    pub trigger_direction: u8,
    pub trigger_price_wad: u128,
}
impl_event!(LoanOpened, [0xe4, 0x52, 0xc8, 0x0d, 0x73, 0x1f, 0xb6, 0x9c]);

/// A loan was repaid in full and its collateral released. Emitted by
/// `RepayLoan`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct LoanRepaid {
    pub pool: Pubkey,
    pub loan: Pubkey,
    pub borrower: Pubkey,
    pub debt_principal: u128,
    /// Principal + accrued interest actually transferred back into the vault.
    pub total_owed: u64,
}
impl_event!(LoanRepaid, [0xe5, 0x88, 0x14, 0x6e, 0xab, 0x30, 0x5f, 0xd2]);

/// A loan was liquidated in-flight during a swap. Emitted by `Swap`, once per
/// liquidated position.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct LoanLiquidated {
    pub pool: Pubkey,
    pub loan: Pubkey,
    pub borrower: Pubkey,
    pub sides: u8,
    pub collateral_amount: u128,
    pub debt_principal: u128,
    pub trigger_price_wad: u128,
}
impl_event!(
    LoanLiquidated,
    [0xe6, 0x71, 0xae, 0x39, 0x02, 0xcd, 0x8b, 0x47]
);

/// A swap committed. Emitted by `Swap` after the liquidation cascade settles.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct SwapExecuted {
    pub pool: Pubkey,
    pub user: Pubkey,
    /// true = A in / B out, false = B in / A out.
    pub a_to_b: bool,
    pub amount_in: u64,
    pub amount_out: u64,
    /// Number of loans liquidated as part of this swap.
    pub liquidations: u32,
    /// Protocol-fee skim credited to the treasury on the input side.
    pub protocol_fee: u64,
}
impl_event!(
    SwapExecuted,
    [0xe7, 0x4d, 0x9f, 0x60, 0xb8, 0x15, 0x2a, 0xe3]
);

/// Accumulated protocol fees were drained to the authority. Emitted by
/// `ClaimProtocolFees`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProtocolFeesClaimed {
    pub pool: Pubkey,
    pub authority: Pubkey,
    pub amount_a: u64,
    pub amount_b: u64,
}
impl_event!(
    ProtocolFeesClaimed,
    [0xe8, 0x26, 0x53, 0xca, 0x7c, 0x91, 0x0e, 0xbf]
);

/// The pool authority was rotated or renounced. Emitted by `TransferAuthority`.
/// `new_authority == Pubkey::default()` signals a permanent renounce.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct AuthorityTransferred {
    pub pool: Pubkey,
    pub old_authority: Pubkey,
    pub new_authority: Pubkey,
}
impl_event!(
    AuthorityTransferred,
    [0xe9, 0x3a, 0xd7, 0x84, 0x1b, 0x6f, 0xc2, 0x50]
);

/// A liquidated borrower reclaimed the rent on their tombstoned loan. Emitted
/// by `ClaimLiquidatedRent`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct LiquidatedRentClaimed {
    pub pool: Pubkey,
    pub loan: Pubkey,
    pub borrower: Pubkey,
}
impl_event!(
    LiquidatedRentClaimed,
    [0xea, 0x5c, 0x08, 0x93, 0xe1, 0x47, 0xba, 0x2d]
);

/// Pool parameters were retuned. Emitted by `UpdatePoolSettings`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct PoolSettingsUpdated {
    pub pool: Pubkey,
    pub swap_fee_bps: u16,
    pub protocol_fee_bps: u16,
    pub liq_ratio_bps: u16,
    pub liq_penalty_bps: u16,
    pub max_ltv_bps: u16,
    pub interest_base_bps_per_year: u16,
    pub interest_slope1_bps_per_year: u16,
    pub interest_slope2_bps_per_year: u16,
    pub interest_kink_bps: u16,
}
impl_event!(
    PoolSettingsUpdated,
    [0xeb, 0x7e, 0x31, 0xa5, 0x4f, 0xd0, 0x68, 0x9b]
);

#[cfg(test)]
mod tests {
    use super::*;

    /// A serialized event leads with its discriminator and round-trips the
    /// payload via borsh from byte 8 onward — exactly the layout an off-chain
    /// consumer decodes.
    fn assert_wire_format<E>(event: &E)
    where
        E: Event + BorshDeserialize + core::fmt::Debug + PartialEq,
    {
        let mut buf = Vec::new();
        buf.extend_from_slice(&E::DISCRIMINATOR);
        event.serialize(&mut buf).unwrap();

        assert_eq!(&buf[..8], &E::DISCRIMINATOR, "discriminator prefix");
        let decoded = E::try_from_slice(&buf[8..]).unwrap();
        assert_eq!(&decoded, event, "payload round-trip");
    }

    #[test]
    fn events_round_trip() {
        assert_wire_format(&PoolInitialized {
            pool: Pubkey::new_unique(),
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            swap_fee_bps: 30,
            protocol_fee_bps: 5,
            liq_ratio_bps: 11_000,
            liq_penalty_bps: 500,
            max_ltv_bps: 8_000,
            interest_base_bps_per_year: 200,
            interest_slope1_bps_per_year: 400,
            interest_slope2_bps_per_year: 6_000,
            interest_kink_bps: 8_000,
        });
        assert_wire_format(&LiquidityAdded {
            pool: Pubkey::new_unique(),
            user: Pubkey::new_unique(),
            amount_a_in: 1_000,
            amount_b_in: 2_000,
            lp_minted: 1_414,
        });
        assert_wire_format(&LiquidityRemoved {
            pool: Pubkey::new_unique(),
            user: Pubkey::new_unique(),
            lp_burned: 500,
            amount_a_out: 400,
            amount_b_out: 600,
        });
        assert_wire_format(&LoanOpened {
            pool: Pubkey::new_unique(),
            loan: Pubkey::new_unique(),
            borrower: Pubkey::new_unique(),
            nonce: 7,
            sides: 1,
            collateral_amount: 10_000,
            debt_amount: 5_000,
            band_id: 42,
            trigger_direction: 0,
            trigger_price_wad: 123_456_789_000_000_000,
        });
        assert_wire_format(&LoanRepaid {
            pool: Pubkey::new_unique(),
            loan: Pubkey::new_unique(),
            borrower: Pubkey::new_unique(),
            debt_principal: 5_000,
            total_owed: 5_123,
        });
        assert_wire_format(&LoanLiquidated {
            pool: Pubkey::new_unique(),
            loan: Pubkey::new_unique(),
            borrower: Pubkey::new_unique(),
            sides: 0,
            collateral_amount: 10_000,
            debt_principal: 5_000,
            trigger_price_wad: 1_000_000_000_000_000_000,
        });
        assert_wire_format(&SwapExecuted {
            pool: Pubkey::new_unique(),
            user: Pubkey::new_unique(),
            a_to_b: true,
            amount_in: 100,
            amount_out: 98,
            liquidations: 2,
            protocol_fee: 1,
        });
        assert_wire_format(&ProtocolFeesClaimed {
            pool: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            amount_a: 10,
            amount_b: 20,
        });
        assert_wire_format(&AuthorityTransferred {
            pool: Pubkey::new_unique(),
            old_authority: Pubkey::new_unique(),
            new_authority: Pubkey::default(),
        });
        assert_wire_format(&LiquidatedRentClaimed {
            pool: Pubkey::new_unique(),
            loan: Pubkey::new_unique(),
            borrower: Pubkey::new_unique(),
        });
        assert_wire_format(&PoolSettingsUpdated {
            pool: Pubkey::new_unique(),
            swap_fee_bps: 25,
            protocol_fee_bps: 5,
            liq_ratio_bps: 12_000,
            liq_penalty_bps: 700,
            max_ltv_bps: 7_500,
            interest_base_bps_per_year: 100,
            interest_slope1_bps_per_year: 300,
            interest_slope2_bps_per_year: 5_000,
            interest_kink_bps: 8_500,
        });
    }

    /// All event discriminators must be distinct, and disjoint from the
    /// account-discriminator namespace (which never leads with `0xe_`).
    #[test]
    fn discriminators_are_unique() {
        let discs = [
            PoolInitialized::DISCRIMINATOR,
            LiquidityAdded::DISCRIMINATOR,
            LiquidityRemoved::DISCRIMINATOR,
            LoanOpened::DISCRIMINATOR,
            LoanRepaid::DISCRIMINATOR,
            LoanLiquidated::DISCRIMINATOR,
            SwapExecuted::DISCRIMINATOR,
            ProtocolFeesClaimed::DISCRIMINATOR,
            AuthorityTransferred::DISCRIMINATOR,
            LiquidatedRentClaimed::DISCRIMINATOR,
            PoolSettingsUpdated::DISCRIMINATOR,
        ];
        for (i, a) in discs.iter().enumerate() {
            assert_eq!(a[0] & 0xf0, 0xe0, "event disc must lead with 0xe_");
            for (j, b) in discs.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "discriminators {i} and {j} collide");
                }
            }
        }
    }
}
