use solana_program::program_error::ProgramError;
use thiserror::Error;

/// All errors emitted by the chiefliquidity program.
///
/// Numbering is part of the ABI — only **append** new variants; never reorder
/// or delete. Off-chain consumers map `ProgramError::Custom(n)` back to a
/// variant by index.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiquidityError {
    // --- generic ---
    #[error("Invalid instruction data")]
    InvalidInstruction,

    #[error("Account already initialized")]
    AlreadyInitialized,

    #[error("Account not initialized")]
    NotInitialized,

    #[error("Invalid PDA")]
    InvalidPDA,

    #[error("Account data too small")]
    AccountDataTooSmall,

    #[error("Invalid account owner")]
    InvalidAccountOwner,

    #[error("Missing required signer")]
    MissingRequiredSigner,

    #[error("System program required")]
    MissingSystemProgram,

    #[error("Authority has been renounced")]
    AuthorityRenounced,

    #[error("Invalid authority")]
    InvalidAuthority,

    // --- pool / mint / vault ---
    #[error("Invalid pool")]
    InvalidPool,

    #[error("Invalid pool mint")]
    InvalidPoolMint,

    #[error("Invalid vault")]
    InvalidVault,

    #[error("Invalid LP mint")]
    InvalidLpMint,

    #[error("Invalid token program")]
    InvalidTokenProgram,

    #[error("Invalid mint - must be Token or Token 2022")]
    InvalidMintProgram,

    #[error("Token mint has a dangerous extension (PermanentDelegate, TransferHook, TransferFee)")]
    UnsupportedMintExtension,

    #[error("Mint A and Mint B must differ")]
    MintsMustDiffer,

    #[error("Mint A must compare lexicographically less than Mint B")]
    MintsNotSorted,

    #[error("Setting value exceeds maximum allowed")]
    SettingExceedsMaximum,

    #[error("Invalid curve kind")]
    InvalidCurveKind,

    // --- math / amounts ---
    #[error("Math overflow")]
    MathOverflow,

    #[error("Math underflow")]
    MathUnderflow,

    #[error("Zero amount not allowed")]
    ZeroAmount,

    #[error("Reserves are zero or pool not seeded")]
    ZeroReserves,

    // --- swap / liquidity ---
    #[error("Slippage exceeded - output below user min_out")]
    SlippageExceeded,

    #[error("Insufficient executable liquidity to satisfy swap")]
    InsufficientExecutableLiquidity,

    // --- lending ---
    #[error("Loan-to-value ratio exceeds maximum allowed")]
    LtvExceedsMax,

    #[error("Loan is not in an open state")]
    LoanNotOpen,

    #[error("Loan is not currently liquidatable at supplied price")]
    LoanNotLiquidatable,

    #[error("Invalid sides encoding for loan")]
    InvalidSidesEncoding,

    #[error("Debt remains after repay - cannot close loan")]
    DebtRemainsAfterRepay,

    // --- liquidation context (swap §6.5) ---
    #[error("Invalid liquidation context - account ordering or band mismatch")]
    InvalidLiquidationContext,

    #[error("Loan-link chain inconsistency - prev/next pointer mismatch")]
    LinkChainBroken,

    #[error("Crossed band must supply all of its links - chain is incomplete")]
    IncompleteBandWalk,

    #[error("Sentinel link's trigger price does not bound the swap path")]
    SentinelMissing,

    #[error("Band PDA does not match supplied (pool, direction, band_id)")]
    BandMismatch,

    #[error("Band has reached its capacity - no more loans can open in this price bucket")]
    BandFull,

    #[error("Swap would require more liquidations than the per-tx cap permits")]
    TooManyLiquidationsRequired,

    // --- invariant violations (should not occur — sanity guards) ---
    #[error("Pool is insolvent post-liquidation - real reserves cannot cover output")]
    Insolvent,

    // --- protocol fees ---
    #[error("Protocol-fee destination is not owned by the fixed fee recipient")]
    InvalidFeeRecipient,
}

impl From<LiquidityError> for ProgramError {
    fn from(e: LiquidityError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
