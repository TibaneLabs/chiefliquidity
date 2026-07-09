/**
 * ChiefLiquidity E2E Tests
 *
 * Run against a local test validator with the program deployed.
 *
 * Setup:
 *   ./scripts/run-e2e-tests.sh        (builds, starts validator, runs this)
 * or manually:
 *   1. cargo build-sbf --workspace
 *   2. solana-test-validator --upgradeable-program ChiefQnUMyz7V1U9odcoxCar66ngVZn1wXFDecnN7yQw \
 *        target/deploy/chiefliquidity.so ~/.config/solana/id.json
 *   3. cd tests/typescript && npm install && npm test
 *
 * Unlike the Rust integration tests (solana-program-test, in-process bank),
 * this suite exercises the real client path over RPC: transaction building,
 * the off-chain swap "router" (Pool band-bitmap walk + getProgramAccounts
 * band enumeration), and live-validator clock/slot behavior.
 */

import {
  ComputeBudgetProgram,
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
  LAMPORTS_PER_SOL,
  sendAndConfirmTransaction,
} from '@solana/web3.js';
import {
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  createMint,
  createInitializeMintInstruction,
  createInitializeTransferFeeConfigInstruction,
  getAssociatedTokenAddressSync,
  createAssociatedTokenAccountInstruction,
  createAssociatedTokenAccountIdempotentInstruction,
  getAccount,
  getMint,
  getMintLen,
  mintTo,
  ExtensionType,
} from '@solana/spl-token';
import bs58 from 'bs58';
import * as fs from 'fs';
import * as os from 'os';

/**
 * Load the program's upgrade-authority keypair (the ClaimProtocolFees claimant).
 * On the local validator the program is deployed with `~/.config/solana/id.json`
 * as upgrade authority (see scripts/run-e2e-tests.sh).
 */
function loadUpgradeAuthority(): Keypair {
  const path = `${os.homedir()}/.config/solana/id.json`;
  const secret = Uint8Array.from(JSON.parse(fs.readFileSync(path, 'utf8')));
  return Keypair.fromSecretKey(secret);
}

// ===== Program constants (must match programs/chiefliquidity/src) =====

const PROGRAM_ID = new PublicKey('ChiefQnUMyz7V1U9odcoxCar66ngVZn1wXFDecnN7yQw');

const POOL_SEED = Buffer.from('pool');
const VAULT_A_SEED = Buffer.from('vault_a');
const VAULT_B_SEED = Buffer.from('vault_b');
const LP_MINT_SEED = Buffer.from('lp_mint');
const LOAN_SEED = Buffer.from('loan');
const BAND_SEED = Buffer.from('band');

const LOAN_DISCRIMINATOR = Buffer.from([0xb2, 0x7e, 0x3c, 0xa0, 0x91, 0x4d, 0x8e, 0x55]);

const WAD = 10n ** 18n;
const BPS = 10_000n;
const LOG2_WAD = 59n;
const BAND_OFFSET = 64n;
const MAX_LIQ_PER_SWAP = 8;
const BAND_MAX_LOANS = 64;

// Account sizes (borsh)
const POOL_LEN = 434;
const LOAN_LEN = 210;
const BAND_LEN = 88;

// Loan field offsets (see state.rs / DESIGN.md §5.2)
const LOAN_OFF = {
  pool: 8,
  borrower: 40,
  nonce: 72,
  sides: 81,
  collateral: 82,
  debtPrincipal: 98,
  triggerPriceWad: 138,
  triggerDirection: 154,
  status: 155,
  bandId: 162,
};

enum Ix {
  InitializePool = 0,
  AddLiquidity = 1,
  RemoveLiquidity = 2,
  OpenLoan = 3,
  RepayLoan = 4,
  ClaimProtocolFees = 5,
  ClaimLiquidatedRent = 6,
  Swap = 7,
}

// BPF upgradeable loader — owns the ProgramData account that records the
// program's upgrade authority (the ClaimProtocolFees claimant).
const BPF_LOADER_UPGRADEABLE = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111');
const [PROGRAM_DATA_ADDRESS] = PublicKey.findProgramAddressSync(
  [PROGRAM_ID.toBuffer()], BPF_LOADER_UPGRADEABLE);

// LiquidityError discriminants (error.rs order — append-only ABI)
enum Err {
  InvalidInstruction = 0,
  AlreadyInitialized = 1,
  NotInitialized = 2,
  InvalidPDA = 3,
  AccountDataTooSmall = 4,
  InvalidAccountOwner = 5,
  MissingRequiredSigner = 6,
  MissingSystemProgram = 7,
  AuthorityRenounced = 8,
  InvalidAuthority = 9,
  InvalidPool = 10,
  InvalidPoolMint = 11,
  InvalidVault = 12,
  InvalidLpMint = 13,
  InvalidTokenProgram = 14,
  InvalidMintProgram = 15,
  UnsupportedMintExtension = 16,
  MintsMustDiffer = 17,
  MintsNotSorted = 18,
  SettingExceedsMaximum = 19,
  InvalidCurveKind = 20,
  MathOverflow = 21,
  MathUnderflow = 22,
  ZeroAmount = 23,
  ZeroReserves = 24,
  SlippageExceeded = 25,
  InsufficientExecutableLiquidity = 26,
  LtvExceedsMax = 27,
  LoanNotOpen = 28,
  LoanNotLiquidatable = 29,
  InvalidSidesEncoding = 30,
  DebtRemainsAfterRepay = 31,
  InvalidLiquidationContext = 32,
  LinkChainBroken = 33,
  IncompleteBandWalk = 34,
  SentinelMissing = 35,
  BandMismatch = 36,
  BandFull = 37,
  TooManyLiquidationsRequired = 38,
  Insolvent = 39,
}

const COLL_A = 0; // collateral A, debt B → OnFall
const COLL_B = 1; // collateral B, debt A → OnRise
const DIR_FALL = 0;
const DIR_RISE = 1;

// ===== Small helpers =====

function writeU128LE(buf: Buffer, val: bigint, off: number) {
  buf.writeBigUInt64LE(val & 0xffffffffffffffffn, off);
  buf.writeBigUInt64LE((val >> 64n) & 0xffffffffffffffffn, off + 8);
}

function readU128LE(buf: Buffer, off: number): bigint {
  return buf.readBigUInt64LE(off) | (buf.readBigUInt64LE(off + 8) << 64n);
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/** Poll until `cond` is true (used to settle 'processed'-commitment bulk ops). */
async function waitFor(cond: () => Promise<boolean>, what: string, timeoutMs = 15_000): Promise<void> {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    if (await cond()) return;
    await sleep(250);
  }
  throw new Error(`timeout waiting for ${what}`);
}

/** floor(sqrt(n)) for bigint */
function isqrt(n: bigint): bigint {
  if (n < 2n) return n;
  let x = n, y = (x + 1n) / 2n;
  while (y < x) { x = y; y = (x + n / x) / 2n; }
  return x;
}

// Trigger math — must match math.rs::recompute_trigger exactly (floor division).
function recomputeTrigger(
  sides: number,
  collateral: bigint,
  debt: bigint,
  liqRatioBps: bigint,
): { triggerWad: bigint; direction: number } {
  if (sides === COLL_A) {
    return {
      triggerWad: (debt * liqRatioBps * WAD) / (collateral * BPS),
      direction: DIR_FALL,
    };
  }
  return {
    triggerWad: (collateral * BPS * WAD) / (debt * liqRatioBps),
    direction: DIR_RISE,
  };
}

// math.rs::band_id_for_trigger: floor(log2(x)) - log2(WAD) + offset
function bandIdForTrigger(triggerWad: bigint): number {
  if (triggerWad <= 0n) throw new Error('zero trigger');
  const log2x = BigInt(triggerWad.toString(2).length - 1);
  return Number(log2x + BAND_OFFSET - LOG2_WAD);
}

function bitmapIsSet(bitmap: Buffer, bandId: number): boolean {
  if (bandId > 127) return false;
  return (bitmap[bandId >> 3] & (1 << (bandId & 7))) !== 0;
}

// CPMM quote — mirrors math.rs::cpmm_quote_out for assertions.
function cpmmQuoteOut(amountIn: bigint, reserveIn: bigint, reserveOut: bigint, feeBps: bigint): bigint {
  const inAfterFee = (amountIn * (BPS - feeBps)) / BPS;
  return (inAfterFee * reserveOut) / (reserveIn + inAfterFee);
}

// ===== State decoders =====

interface PoolState {
  mintA: PublicKey;
  mintB: PublicKey;
  vaultA: PublicKey;
  vaultB: PublicKey;
  lpMint: PublicKey;
  authority: PublicKey;
  totalDebtA: bigint;
  totalDebtB: bigint;
  totalCollateralA: bigint;
  totalCollateralB: bigint;
  swapFeeBps: number;
  protocolFeeBps: number;
  liqRatioBps: number;
  maxLtvBps: number;
  borrowIndexAWad: bigint;
  borrowIndexBWad: bigint;
  openLoans: bigint;
  nextLoanNonce: bigint;
  protocolFeesA: bigint;
  protocolFeesB: bigint;
  bandBitmapFall: Buffer;
  bandBitmapRise: Buffer;
}

function decodePool(data: Buffer): PoolState {
  if (data.length !== POOL_LEN) throw new Error(`pool size ${data.length} != ${POOL_LEN}`);
  return {
    mintA: new PublicKey(data.subarray(8, 40)),
    mintB: new PublicKey(data.subarray(40, 72)),
    vaultA: new PublicKey(data.subarray(72, 104)),
    vaultB: new PublicKey(data.subarray(104, 136)),
    lpMint: new PublicKey(data.subarray(136, 168)),
    authority: new PublicKey(data.subarray(168, 200)),
    totalDebtA: readU128LE(data, 204),
    totalDebtB: readU128LE(data, 220),
    totalCollateralA: readU128LE(data, 236),
    totalCollateralB: readU128LE(data, 252),
    swapFeeBps: data.readUInt16LE(269),
    protocolFeeBps: data.readUInt16LE(271),
    liqRatioBps: data.readUInt16LE(276),
    maxLtvBps: data.readUInt16LE(278),
    borrowIndexAWad: readU128LE(data, 290),
    borrowIndexBWad: readU128LE(data, 306),
    openLoans: data.readBigUInt64LE(330),
    nextLoanNonce: data.readBigUInt64LE(338),
    protocolFeesA: data.readBigUInt64LE(354),
    protocolFeesB: data.readBigUInt64LE(362),
    bandBitmapFall: Buffer.from(data.subarray(370, 386)),
    bandBitmapRise: Buffer.from(data.subarray(386, 402)),
  };
}

interface LoanState {
  pool: PublicKey;
  borrower: PublicKey;
  nonce: bigint;
  sides: number;
  collateral: bigint;
  debtPrincipal: bigint;
  triggerPriceWad: bigint;
  triggerDirection: number;
  status: number;
  bandId: number;
}

function decodeLoan(data: Buffer): LoanState {
  if (data.length !== LOAN_LEN) throw new Error(`loan size ${data.length} != ${LOAN_LEN}`);
  return {
    pool: new PublicKey(data.subarray(LOAN_OFF.pool, LOAN_OFF.pool + 32)),
    borrower: new PublicKey(data.subarray(LOAN_OFF.borrower, LOAN_OFF.borrower + 32)),
    nonce: data.readBigUInt64LE(LOAN_OFF.nonce),
    sides: data[LOAN_OFF.sides],
    collateral: readU128LE(data, LOAN_OFF.collateral),
    debtPrincipal: readU128LE(data, LOAN_OFF.debtPrincipal),
    triggerPriceWad: readU128LE(data, LOAN_OFF.triggerPriceWad),
    triggerDirection: data[LOAN_OFF.triggerDirection],
    status: data[LOAN_OFF.status],
    bandId: data.readUInt32LE(LOAN_OFF.bandId),
  };
}

interface BandState {
  pool: PublicKey;
  bandId: number;
  direction: number;
  count: number;
}

function decodeBand(data: Buffer): BandState {
  if (data.length !== BAND_LEN) throw new Error(`band size ${data.length} != ${BAND_LEN}`);
  return {
    pool: new PublicKey(data.subarray(8, 40)),
    bandId: data.readUInt32LE(40),
    direction: data[44],
    count: data.readUInt32LE(48),
  };
}

// ===== Instruction builders =====

interface PoolParams {
  swapFeeBps: number;
  protocolFeeBps: number;
  liqRatioBps: number;
  maxLtvBps: number;
  interestBaseBps: number;
  interestSlope1Bps: number;
  interestSlope2Bps: number;
  interestKinkBps: number;
}

const DEFAULT_PARAMS: PoolParams = {
  swapFeeBps: 30,
  protocolFeeBps: 5,
  liqRatioBps: 11_000,
  maxLtvBps: 8_000,
  interestBaseBps: 0,
  interestSlope1Bps: 400,
  interestSlope2Bps: 30_000,
  interestKinkBps: 8_000,
};

function encodeParams(variant: Ix, p: PoolParams): Buffer {
  const data = Buffer.alloc(1 + 8 * 2);
  data.writeUInt8(variant, 0);
  data.writeUInt16LE(p.swapFeeBps, 1);
  data.writeUInt16LE(p.protocolFeeBps, 3);
  data.writeUInt16LE(p.liqRatioBps, 5);
  data.writeUInt16LE(p.maxLtvBps, 7);
  data.writeUInt16LE(p.interestBaseBps, 9);
  data.writeUInt16LE(p.interestSlope1Bps, 11);
  data.writeUInt16LE(p.interestSlope2Bps, 13);
  data.writeUInt16LE(p.interestKinkBps, 15);
  return data;
}

// ===== Error extraction =====

function customErrorCode(e: any): number | null {
  const text = `${e.message ?? ''}\n${(e.logs ?? e.transactionLogs ?? []).join('\n')}`;
  const m = text.match(/custom program error: (0x[0-9a-fA-F]+)/);
  if (m) return parseInt(m[1], 16);
  const m2 = text.match(/"Custom":\s*(\d+)/);
  if (m2) return parseInt(m2[1], 10);
  return null;
}

// ===== Test context =====

class Ctx {
  connection: Connection;
  payer: Keypair;
  tokenProgram: PublicKey;
  mintA!: PublicKey;
  mintB!: PublicKey;
  pool!: PublicKey;
  vaultA!: PublicKey;
  vaultB!: PublicKey;
  lpMint!: PublicKey;
  authority: Keypair;

  constructor(connection: Connection, payer: Keypair, tokenProgram: PublicKey) {
    this.connection = connection;
    this.payer = payer;
    this.tokenProgram = tokenProgram;
    this.authority = payer;
  }

  /** Create two fresh sorted mints (A: 9 decimals, B: 6) and derive pool PDAs. */
  async setup(): Promise<void> {
    let a = Keypair.generate();
    let b = Keypair.generate();
    if (Buffer.compare(a.publicKey.toBuffer(), b.publicKey.toBuffer()) > 0) {
      [a, b] = [b, a];
    }
    await createMint(this.connection, this.payer, this.payer.publicKey, null, 9,
      a, { commitment: 'confirmed' }, this.tokenProgram);
    await createMint(this.connection, this.payer, this.payer.publicKey, null, 6,
      b, { commitment: 'confirmed' }, this.tokenProgram);
    this.mintA = a.publicKey;
    this.mintB = b.publicKey;
    [this.pool] = PublicKey.findProgramAddressSync(
      [POOL_SEED, this.mintA.toBuffer(), this.mintB.toBuffer()], PROGRAM_ID);
    [this.vaultA] = PublicKey.findProgramAddressSync([VAULT_A_SEED, this.pool.toBuffer()], PROGRAM_ID);
    [this.vaultB] = PublicKey.findProgramAddressSync([VAULT_B_SEED, this.pool.toBuffer()], PROGRAM_ID);
    [this.lpMint] = PublicKey.findProgramAddressSync([LP_MINT_SEED, this.pool.toBuffer()], PROGRAM_ID);
  }

  loanPda(borrower: PublicKey, nonce: bigint): PublicKey {
    const nb = Buffer.alloc(8);
    nb.writeBigUInt64LE(nonce);
    return PublicKey.findProgramAddressSync(
      [LOAN_SEED, this.pool.toBuffer(), borrower.toBuffer(), nb], PROGRAM_ID)[0];
  }

  bandPda(direction: number, bandId: number): PublicKey {
    const bb = Buffer.alloc(4);
    bb.writeUInt32LE(bandId);
    return PublicKey.findProgramAddressSync(
      [BAND_SEED, this.pool.toBuffer(), Buffer.from([direction]), bb], PROGRAM_ID)[0];
  }

  ata(owner: PublicKey, mint: PublicKey): PublicKey {
    return getAssociatedTokenAddressSync(mint, owner, false, this.tokenProgram);
  }

  async send(ixs: TransactionInstruction[], signers: Keypair[],
             commitment: 'confirmed' | 'processed' = 'confirmed'): Promise<string> {
    const tx = new Transaction().add(...ixs);
    return sendAndConfirmTransaction(this.connection, tx, [this.payer, ...signers.filter(s => s !== this.payer)],
      { commitment, skipPreflight: false });
  }

  /** New keypair funded with SOL + both-token ATAs (+LP ATA), minted balances. */
  async newUser(sol: number, tokenA: bigint, tokenB: bigint): Promise<Keypair> {
    const user = Keypair.generate();
    const ixs: TransactionInstruction[] = [
      SystemProgram.transfer({
        fromPubkey: this.payer.publicKey, toPubkey: user.publicKey,
        lamports: sol * LAMPORTS_PER_SOL,
      }),
      createAssociatedTokenAccountInstruction(this.payer.publicKey,
        this.ata(user.publicKey, this.mintA), user.publicKey, this.mintA,
        this.tokenProgram, ASSOCIATED_TOKEN_PROGRAM_ID),
      createAssociatedTokenAccountInstruction(this.payer.publicKey,
        this.ata(user.publicKey, this.mintB), user.publicKey, this.mintB,
        this.tokenProgram, ASSOCIATED_TOKEN_PROGRAM_ID),
      createAssociatedTokenAccountInstruction(this.payer.publicKey,
        this.ata(user.publicKey, this.lpMint), user.publicKey, this.lpMint,
        this.tokenProgram, ASSOCIATED_TOKEN_PROGRAM_ID),
    ];
    await this.send(ixs, []);
    if (tokenA > 0n) {
      await mintTo(this.connection, this.payer, this.mintA, this.ata(user.publicKey, this.mintA),
        this.payer, tokenA, [], { commitment: 'confirmed' }, this.tokenProgram);
    }
    if (tokenB > 0n) {
      await mintTo(this.connection, this.payer, this.mintB, this.ata(user.publicKey, this.mintB),
        this.payer, tokenB, [], { commitment: 'confirmed' }, this.tokenProgram);
    }
    return user;
  }

  // ---- instruction wrappers ----

  ixInitializePool(params: PoolParams, mintA?: PublicKey, mintB?: PublicKey): TransactionInstruction {
    return new TransactionInstruction({
      programId: PROGRAM_ID,
      keys: [
        { pubkey: this.pool, isSigner: false, isWritable: true },
        { pubkey: mintA ?? this.mintA, isSigner: false, isWritable: false },
        { pubkey: mintB ?? this.mintB, isSigner: false, isWritable: false },
        { pubkey: this.vaultA, isSigner: false, isWritable: true },
        { pubkey: this.vaultB, isSigner: false, isWritable: true },
        { pubkey: this.lpMint, isSigner: false, isWritable: true },
        { pubkey: this.authority.publicKey, isSigner: true, isWritable: true },
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
        { pubkey: this.tokenProgram, isSigner: false, isWritable: false },
        { pubkey: new PublicKey('SysvarRent111111111111111111111111111111111'), isSigner: false, isWritable: false },
      ],
      // Pools are immutable/authority-less: InitializePool takes no args.
      // `params` is accepted for call-site compatibility but ignored.
      data: Buffer.from([Ix.InitializePool]),
    });
  }

  async initializePool(params: PoolParams = DEFAULT_PARAMS): Promise<void> {
    await this.send([this.ixInitializePool(params)], [this.authority]);
  }

  async addLiquidity(user: Keypair, aMax: bigint, bMax: bigint, minLp: bigint): Promise<void> {
    const data = Buffer.alloc(1 + 24);
    data.writeUInt8(Ix.AddLiquidity, 0);
    data.writeBigUInt64LE(aMax, 1);
    data.writeBigUInt64LE(bMax, 9);
    data.writeBigUInt64LE(minLp, 17);
    const ix = new TransactionInstruction({
      programId: PROGRAM_ID,
      keys: [
        { pubkey: this.pool, isSigner: false, isWritable: true },
        { pubkey: this.vaultA, isSigner: false, isWritable: true },
        { pubkey: this.vaultB, isSigner: false, isWritable: true },
        { pubkey: this.lpMint, isSigner: false, isWritable: true },
        { pubkey: this.ata(user.publicKey, this.mintA), isSigner: false, isWritable: true },
        { pubkey: this.ata(user.publicKey, this.mintB), isSigner: false, isWritable: true },
        { pubkey: this.ata(user.publicKey, this.lpMint), isSigner: false, isWritable: true },
        { pubkey: user.publicKey, isSigner: true, isWritable: false },
        { pubkey: this.mintA, isSigner: false, isWritable: false },
        { pubkey: this.mintB, isSigner: false, isWritable: false },
        { pubkey: this.tokenProgram, isSigner: false, isWritable: false },
      ],
      data,
    });
    await this.send([ix], [user]);
  }

  async removeLiquidity(user: Keypair, lpAmount: bigint, minA: bigint, minB: bigint): Promise<void> {
    const data = Buffer.alloc(1 + 24);
    data.writeUInt8(Ix.RemoveLiquidity, 0);
    data.writeBigUInt64LE(lpAmount, 1);
    data.writeBigUInt64LE(minA, 9);
    data.writeBigUInt64LE(minB, 17);
    const ix = new TransactionInstruction({
      programId: PROGRAM_ID,
      keys: [
        { pubkey: this.pool, isSigner: false, isWritable: true },
        { pubkey: this.vaultA, isSigner: false, isWritable: true },
        { pubkey: this.vaultB, isSigner: false, isWritable: true },
        { pubkey: this.lpMint, isSigner: false, isWritable: true },
        { pubkey: this.ata(user.publicKey, this.mintA), isSigner: false, isWritable: true },
        { pubkey: this.ata(user.publicKey, this.mintB), isSigner: false, isWritable: true },
        { pubkey: this.ata(user.publicKey, this.lpMint), isSigner: false, isWritable: true },
        { pubkey: user.publicKey, isSigner: true, isWritable: false },
        { pubkey: this.mintA, isSigner: false, isWritable: false },
        { pubkey: this.mintB, isSigner: false, isWritable: false },
        { pubkey: this.tokenProgram, isSigner: false, isWritable: false },
      ],
      data,
    });
    await this.send([ix], [user]);
  }

  /**
   * Open a loan. Computes nonce / trigger / band exactly as the program does.
   * Pass `nonceOverride` to test stale-nonce rejection.
   */
  async openLoan(
    borrower: Keypair,
    sides: number,
    collateral: bigint,
    debt: bigint,
    opts: { nonceOverride?: bigint; commitment?: 'confirmed' | 'processed'; knownNonce?: bigint } = {},
  ): Promise<{ loan: PublicKey; band: PublicKey; bandId: number; direction: number; nonce: bigint }> {
    let nonce = opts.knownNonce;
    if (nonce === undefined) {
      nonce = (await this.poolState()).nextLoanNonce;
    }
    const sentNonce = opts.nonceOverride ?? nonce;
    const pool = await this.poolState();
    const { triggerWad, direction } = recomputeTrigger(sides, collateral, debt, BigInt(pool.liqRatioBps));
    const bandId = bandIdForTrigger(triggerWad);
    const loan = this.loanPda(borrower.publicKey, sentNonce);
    const band = this.bandPda(direction, bandId);

    const data = Buffer.alloc(1 + 1 + 8 + 8 + 8);
    data.writeUInt8(Ix.OpenLoan, 0);
    data.writeUInt8(sides, 1);
    data.writeBigUInt64LE(collateral, 2);
    data.writeBigUInt64LE(debt, 10);
    data.writeBigUInt64LE(sentNonce, 18);
    const ix = new TransactionInstruction({
      programId: PROGRAM_ID,
      keys: [
        { pubkey: this.pool, isSigner: false, isWritable: true },
        { pubkey: this.vaultA, isSigner: false, isWritable: true },
        { pubkey: this.vaultB, isSigner: false, isWritable: true },
        { pubkey: this.ata(borrower.publicKey, this.mintA), isSigner: false, isWritable: true },
        { pubkey: this.ata(borrower.publicKey, this.mintB), isSigner: false, isWritable: true },
        { pubkey: this.mintA, isSigner: false, isWritable: false },
        { pubkey: this.mintB, isSigner: false, isWritable: false },
        { pubkey: borrower.publicKey, isSigner: true, isWritable: true },
        { pubkey: loan, isSigner: false, isWritable: true },
        { pubkey: band, isSigner: false, isWritable: true },
        { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
        { pubkey: this.tokenProgram, isSigner: false, isWritable: false },
      ],
      data,
    });
    await this.send([ix], [borrower], opts.commitment ?? 'confirmed');
    return { loan, band, bandId, direction, nonce: sentNonce };
  }

  async repayLoan(borrower: Keypair, loan: PublicKey, opts: { signer?: Keypair } = {}): Promise<void> {
    const info = await this.connection.getAccountInfo(loan);
    if (!info) throw new Error('loan account missing');
    const state = decodeLoan(info.data);
    const band = this.bandPda(state.triggerDirection, state.bandId);
    const signer = opts.signer ?? borrower;
    const ix = new TransactionInstruction({
      programId: PROGRAM_ID,
      keys: [
        { pubkey: this.pool, isSigner: false, isWritable: true },
        { pubkey: this.vaultA, isSigner: false, isWritable: true },
        { pubkey: this.vaultB, isSigner: false, isWritable: true },
        { pubkey: this.ata(signer.publicKey, this.mintA), isSigner: false, isWritable: true },
        { pubkey: this.ata(signer.publicKey, this.mintB), isSigner: false, isWritable: true },
        { pubkey: this.mintA, isSigner: false, isWritable: false },
        { pubkey: this.mintB, isSigner: false, isWritable: false },
        { pubkey: signer.publicKey, isSigner: true, isWritable: true },
        { pubkey: loan, isSigner: false, isWritable: true },
        { pubkey: band, isSigner: false, isWritable: true },
        { pubkey: this.tokenProgram, isSigner: false, isWritable: false },
      ],
      data: Buffer.from([Ix.RepayLoan]),
    });
    await this.send([ix], [signer]);
  }

  /**
   * The off-chain "router": enumerate every populated band for the swap's
   * trigger direction from the Pool bitmap, fetch each band's open loans via
   * getProgramAccounts, and build the full (band, loans…) account tail. This
   * is the documented client flow from DESIGN.md §6.2.
   */
  async buildLiquidationContext(aToB: boolean): Promise<{
    boundary: number;
    bands: { bandId: number; loans: PublicKey[] }[];
  }> {
    const pool = await this.poolState();
    const direction = aToB ? DIR_FALL : DIR_RISE;
    const bitmap = aToB ? pool.bandBitmapFall : pool.bandBitmapRise;

    // One fetch of all this pool's loans (RPC caps filters at 4; finer
    // criteria — direction, status, band — are applied client-side).
    const accounts = await this.connection.getProgramAccounts(PROGRAM_ID, {
      commitment: 'confirmed',
      filters: [
        { dataSize: LOAN_LEN },
        { memcmp: { offset: 0, bytes: bs58.encode(LOAN_DISCRIMINATOR) } },
        { memcmp: { offset: LOAN_OFF.pool, bytes: this.pool.toBase58() } },
      ],
    });
    const byBand = new Map<number, PublicKey[]>();
    for (const { pubkey, account } of accounts) {
      const loan = decodeLoan(account.data);
      if (loan.status !== 0 || loan.triggerDirection !== direction) continue;
      const list = byBand.get(loan.bandId) ?? [];
      list.push(pubkey);
      byBand.set(loan.bandId, list);
    }
    const bands: { bandId: number; loans: PublicKey[] }[] = [];
    for (let bandId = 0; bandId <= 127; bandId++) {
      if (!bitmapIsSet(bitmap, bandId)) continue;
      const loans = (byBand.get(bandId) ?? [])
        .sort((x, y) => Buffer.compare(x.toBuffer(), y.toBuffer()));
      bands.push({ bandId, loans });
    }
    // Wide-open boundary: every populated band is supplied, so any cascade
    // depth is covered.
    return { boundary: aToB ? 0 : 127, bands };
  }

  ixSwap(
    user: PublicKey,
    amountIn: bigint,
    minOut: bigint,
    aToB: boolean,
    boundary: number,
    bands: { bandId: number; loans: PublicKey[] }[],
  ): TransactionInstruction {
    const counts = bands.map((b) => b.loans.length);
    const data = Buffer.alloc(1 + 8 + 8 + 1 + 4 + 4 + counts.length);
    let off = 0;
    data.writeUInt8(Ix.Swap, off); off += 1;
    data.writeBigUInt64LE(amountIn, off); off += 8;
    data.writeBigUInt64LE(minOut, off); off += 8;
    data.writeUInt8(aToB ? 1 : 0, off); off += 1;
    data.writeUInt32LE(boundary, off); off += 4;
    data.writeUInt32LE(counts.length, off); off += 4; // Vec<u8> length prefix
    for (const c of counts) { data.writeUInt8(c, off); off += 1; }

    const keys = [
      { pubkey: this.pool, isSigner: false, isWritable: true },
      { pubkey: this.vaultA, isSigner: false, isWritable: true },
      { pubkey: this.vaultB, isSigner: false, isWritable: true },
      { pubkey: this.ata(user, this.mintA), isSigner: false, isWritable: true },
      { pubkey: this.ata(user, this.mintB), isSigner: false, isWritable: true },
      { pubkey: this.mintA, isSigner: false, isWritable: false },
      { pubkey: this.mintB, isSigner: false, isWritable: false },
      { pubkey: user, isSigner: true, isWritable: false },
      { pubkey: this.tokenProgram, isSigner: false, isWritable: false },
    ];
    const direction = aToB ? DIR_FALL : DIR_RISE;
    for (const b of bands) {
      keys.push({ pubkey: this.bandPda(direction, b.bandId), isSigner: false, isWritable: true });
      for (const loan of b.loans) {
        keys.push({ pubkey: loan, isSigner: false, isWritable: true });
      }
    }
    return new TransactionInstruction({ programId: PROGRAM_ID, keys, data });
  }

  /** Swap with an explicitly-supplied liquidation context. */
  async swapRaw(
    user: Keypair, amountIn: bigint, minOut: bigint, aToB: boolean,
    boundary: number, bands: { bandId: number; loans: PublicKey[] }[],
  ): Promise<string> {
    const cu = ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 });
    return this.send([cu, this.ixSwap(user.publicKey, amountIn, minOut, aToB, boundary, bands)], [user]);
  }

  /** Swap with router-built (complete) liquidation context. */
  async swap(user: Keypair, amountIn: bigint, minOut: bigint, aToB: boolean): Promise<string> {
    const { boundary, bands } = await this.buildLiquidationContext(aToB);
    return this.swapRaw(user, amountIn, minOut, aToB, boundary, bands);
  }

  /**
   * Claim protocol fees. Gated on the PROGRAM upgrade authority (pools have
   * none): `authority` must be the program's upgrade authority and the
   * ProgramData account is supplied so the program can read it. Fees land in
   * `authority`'s ATAs (created by the caller).
   */
  async claimProtocolFees(authority: Keypair): Promise<void> {
    const owner = authority.publicKey;
    const ix = new TransactionInstruction({
      programId: PROGRAM_ID,
      keys: [
        { pubkey: this.pool, isSigner: false, isWritable: true },
        { pubkey: this.vaultA, isSigner: false, isWritable: true },
        { pubkey: this.vaultB, isSigner: false, isWritable: true },
        { pubkey: this.ata(owner, this.mintA), isSigner: false, isWritable: true },
        { pubkey: this.ata(owner, this.mintB), isSigner: false, isWritable: true },
        { pubkey: this.mintA, isSigner: false, isWritable: false },
        { pubkey: this.mintB, isSigner: false, isWritable: false },
        { pubkey: authority.publicKey, isSigner: true, isWritable: false },
        { pubkey: PROGRAM_DATA_ADDRESS, isSigner: false, isWritable: false },
        { pubkey: this.tokenProgram, isSigner: false, isWritable: false },
      ],
      data: Buffer.from([Ix.ClaimProtocolFees]),
    });
    await this.send([ix], [authority]);
  }

  async claimLiquidatedRent(borrower: Keypair, loan: PublicKey): Promise<void> {
    const ix = new TransactionInstruction({
      programId: PROGRAM_ID,
      keys: [
        { pubkey: loan, isSigner: false, isWritable: true },
        { pubkey: borrower.publicKey, isSigner: true, isWritable: true },
      ],
      data: Buffer.from([Ix.ClaimLiquidatedRent]),
    });
    await this.send([ix], [borrower]);
  }

  // ---- state readers ----

  async poolState(): Promise<PoolState> {
    const info = await this.connection.getAccountInfo(this.pool, 'confirmed');
    if (!info) throw new Error('pool not found');
    return decodePool(info.data);
  }

  async loanState(loan: PublicKey): Promise<LoanState | null> {
    const info = await this.connection.getAccountInfo(loan, 'confirmed');
    return info && info.data.length === LOAN_LEN ? decodeLoan(info.data) : null;
  }

  async bandState(direction: number, bandId: number): Promise<BandState | null> {
    const info = await this.connection.getAccountInfo(this.bandPda(direction, bandId), 'confirmed');
    return info && info.data.length === BAND_LEN ? decodeBand(info.data) : null;
  }

  async tokenBalance(owner: PublicKey, mint: PublicKey): Promise<bigint> {
    const acc = await getAccount(this.connection, this.ata(owner, mint), 'confirmed', this.tokenProgram);
    return acc.amount;
  }

  async vaultBalances(): Promise<{ a: bigint; b: bigint }> {
    const a = await getAccount(this.connection, this.vaultA, 'confirmed', this.tokenProgram);
    const b = await getAccount(this.connection, this.vaultB, 'confirmed', this.tokenProgram);
    return { a: a.amount, b: b.amount };
  }

  async lpSupply(): Promise<bigint> {
    return (await getMint(this.connection, this.lpMint, 'confirmed', this.tokenProgram)).supply;
  }

  /** initialize + seed: returns the LP user. */
  async setupPoolWithLiquidity(amountA: bigint, amountB: bigint,
                               params: PoolParams = DEFAULT_PARAMS): Promise<Keypair> {
    await this.initializePool(params);
    const lp = await this.newUser(10, amountA * 2n, amountB * 2n);
    await this.addLiquidity(lp, amountA, amountB, 1n);
    return lp;
  }
}

// ===== Assertion helpers =====

function assert(cond: boolean, msg: string) {
  if (!cond) throw new Error(`assertion failed: ${msg}`);
}

function assertEq(actual: any, expected: any, msg: string) {
  const a = typeof actual === 'bigint' ? actual.toString() : JSON.stringify(actual);
  const e = typeof expected === 'bigint' ? expected.toString() : JSON.stringify(expected);
  if (a !== e) throw new Error(`assertion failed: ${msg} — got ${a}, want ${e}`);
}

async function expectError(p: Promise<any>, code: Err, what: string): Promise<void> {
  try {
    await p;
  } catch (e: any) {
    const got = customErrorCode(e);
    if (got === code) return;
    throw new Error(`${what}: expected error ${Err[code]} (${code}), got ${got !== null ? `${Err[got] ?? '?'} (${got})` : e.message}`);
  }
  throw new Error(`${what}: expected error ${Err[code]}, but transaction succeeded`);
}

// ===== Main =====

async function runTests() {
  console.log('=== ChiefLiquidity E2E Tests ===\n');

  const connection = new Connection('http://localhost:8899', 'confirmed');

  try {
    await connection.getVersion();
  } catch (e) {
    console.error('ERROR: Cannot connect to test validator on localhost:8899.');
    console.error('Start one with: ./scripts/run-e2e-tests.sh');
    process.exit(1);
  }
  const programInfo = await connection.getAccountInfo(PROGRAM_ID);
  if (!programInfo) {
    console.error(`ERROR: Program ${PROGRAM_ID.toBase58()} not deployed.`);
    process.exit(1);
  }

  // Master payer funds everything (mints, users) via transfers.
  const payer = Keypair.generate();
  {
    const sig = await connection.requestAirdrop(payer.publicKey, 2_000 * LAMPORTS_PER_SOL);
    await connection.confirmTransaction(sig);
  }

  let passed = 0;
  let failed = 0;

  async function test(name: string, fn: () => Promise<void>) {
    const t0 = Date.now();
    try {
      await fn();
      console.log(`✓ ${name} (${((Date.now() - t0) / 1000).toFixed(1)}s)`);
      passed++;
    } catch (e: any) {
      console.log(`✗ ${name}`);
      console.log(`  Error: ${e.message}`);
      if (e.logs) console.log(`  Logs: ${e.logs.slice(-6).join('\n        ')}`);
      failed++;
    }
  }

  for (const [label, tokenProgramId] of [
    ['Token2022', TOKEN_2022_PROGRAM_ID],
    ['SPL Token', TOKEN_PROGRAM_ID],
  ] as [string, PublicKey][]) {

    console.log(`\n========== ${label} ==========\n`);

    const T = (name: string, fn: (ctx: Ctx) => Promise<void>) =>
      test(`[${label}] ${name}`, async () => {
        const ctx = new Ctx(connection, payer, tokenProgramId);
        await ctx.setup();
        await fn(ctx);
      });

    // ---------- Pool lifecycle ----------

    await T('Initialize pool persists config and creates PDAs', async (ctx) => {
      await ctx.initializePool();
      const pool = await ctx.poolState();
      assertEq(pool.mintA.toBase58(), ctx.mintA.toBase58(), 'mint_a');
      assertEq(pool.mintB.toBase58(), ctx.mintB.toBase58(), 'mint_b');
      // Pools are authority-less: authority is the zero pubkey, not the creator.
      assertEq(pool.authority.toBase58(), PublicKey.default.toBase58(), 'authority is none');
      assertEq(pool.swapFeeBps, 30, 'swap_fee');
      assertEq(pool.liqRatioBps, 11_000, 'liq_ratio');
      assertEq(pool.openLoans, 0n, 'open_loans');
      assertEq(pool.borrowIndexAWad, WAD, 'index_a starts at WAD');
      const { a, b } = await ctx.vaultBalances();
      assertEq(a, 0n, 'vault_a empty');
      assertEq(b, 0n, 'vault_b empty');
      assertEq(await ctx.lpSupply(), 0n, 'lp supply 0');
    });

    await T('Initialize pool: reinitialization rejected', async (ctx) => {
      await ctx.initializePool();
      await expectError(ctx.initializePool(), Err.AlreadyInitialized, 'reinit');
    });

    await T('Initialize pool: unsorted mints rejected', async (ctx) => {
      await ctx.initializePool();
      // Swap mint order in the ix while keeping the (valid) PDA accounts: the
      // sort check fires before the PDA check.
      const ctx2 = new Ctx(connection, payer, tokenProgramId);
      await ctx2.setup();
      await expectError(
        ctx2.send([ctx2.ixInitializePool(DEFAULT_PARAMS, ctx2.mintB, ctx2.mintA)], [payer]),
        Err.MintsNotSorted, 'unsorted mints');
    });

    // (Parameter-bounds test removed: pool economics are fixed program
    // constants baked in by InitializePool — there are no caller-supplied
    // params to validate. Bounds are checked at compile time in the program.)

    if (tokenProgramId.equals(TOKEN_2022_PROGRAM_ID)) {
      await T('Initialize pool: TransferFee mint rejected', async (ctx) => {
        // Build a transfer-fee mint lexicographically ordered against mint_b.
        const feeMintKp = Keypair.generate();
        const mintLen = getMintLen([ExtensionType.TransferFeeConfig]);
        const rent = await connection.getMinimumBalanceForRentExemption(mintLen);
        const tx = new Transaction().add(
          SystemProgram.createAccount({
            fromPubkey: payer.publicKey, newAccountPubkey: feeMintKp.publicKey,
            space: mintLen, lamports: rent, programId: TOKEN_2022_PROGRAM_ID,
          }),
          createInitializeTransferFeeConfigInstruction(
            feeMintKp.publicKey, payer.publicKey, payer.publicKey, 100, 1_000_000n,
            TOKEN_2022_PROGRAM_ID),
          createInitializeMintInstruction(feeMintKp.publicKey, 6, payer.publicKey, null,
            TOKEN_2022_PROGRAM_ID),
        );
        await sendAndConfirmTransaction(connection, tx, [payer, feeMintKp], { commitment: 'confirmed' });

        const feeMint = feeMintKp.publicKey;
        const [mintA, mintB] =
          Buffer.compare(feeMint.toBuffer(), ctx.mintB.toBuffer()) < 0
            ? [feeMint, ctx.mintB] : [ctx.mintB, feeMint];
        const bad = new Ctx(connection, payer, tokenProgramId);
        bad.mintA = mintA;
        bad.mintB = mintB;
        [bad.pool] = PublicKey.findProgramAddressSync(
          [POOL_SEED, mintA.toBuffer(), mintB.toBuffer()], PROGRAM_ID);
        [bad.vaultA] = PublicKey.findProgramAddressSync([VAULT_A_SEED, bad.pool.toBuffer()], PROGRAM_ID);
        [bad.vaultB] = PublicKey.findProgramAddressSync([VAULT_B_SEED, bad.pool.toBuffer()], PROGRAM_ID);
        [bad.lpMint] = PublicKey.findProgramAddressSync([LP_MINT_SEED, bad.pool.toBuffer()], PROGRAM_ID);
        await expectError(bad.initializePool(), Err.UnsupportedMintExtension, 'transfer-fee mint');
      });
    }

    // ---------- Liquidity ----------

    await T('Add liquidity: first deposit mints sqrt(a*b)', async (ctx) => {
      await ctx.initializePool();
      const user = await ctx.newUser(10, 4_000_000n, 1_000_000n);
      await ctx.addLiquidity(user, 4_000_000n, 1_000_000n, 1n);
      const lp = await ctx.tokenBalance(user.publicKey, ctx.lpMint);
      assertEq(lp, isqrt(4_000_000n * 1_000_000n), 'sqrt LP');
      const { a, b } = await ctx.vaultBalances();
      assertEq(a, 4_000_000n, 'vault a');
      assertEq(b, 1_000_000n, 'vault b');
    });

    await T('Add liquidity: second deposit proportional, excess clipped', async (ctx) => {
      const lp1 = await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const supply0 = await ctx.lpSupply();
      const user = await ctx.newUser(10, 100_000_000n, 1_000_000_000n);
      // Offer 4x more B than the ratio needs; program must clip B to 400M.
      await ctx.addLiquidity(user, 100_000_000n, 1_000_000_000n, 1n);
      const balB = await ctx.tokenBalance(user.publicKey, ctx.mintB);
      assertEq(balB, 1_000_000_000n - 400_000_000n, 'B clipped to ratio');
      const lp = await ctx.tokenBalance(user.publicKey, ctx.lpMint);
      assertEq(lp, supply0 / 10n, 'proportional LP (10% of pool)');
      void lp1;
    });

    await T('Add liquidity: slippage and dust-deposit rejected', async (ctx) => {
      await ctx.initializePool();
      const user = await ctx.newUser(10, 10_000_000n, 10_000_000n);
      await expectError(ctx.addLiquidity(user, 100n, 100n, 1n),
        Err.ZeroAmount, 'below MIN_FIRST_DEPOSIT');
      await expectError(ctx.addLiquidity(user, 2_000_000n, 2_000_000n, 10_000_000_000n),
        Err.SlippageExceeded, 'min_lp_out breach');
    });

    await T('Remove liquidity: partial and full round trip', async (ctx) => {
      await ctx.initializePool();
      const user = await ctx.newUser(10, 1_000_000_000n, 4_000_000_000n);
      await ctx.addLiquidity(user, 1_000_000_000n, 4_000_000_000n, 1n);
      const lp = await ctx.tokenBalance(user.publicKey, ctx.lpMint);

      await ctx.removeLiquidity(user, lp / 2n, 1n, 1n);
      const balA1 = await ctx.tokenBalance(user.publicKey, ctx.mintA);
      assert(balA1 >= 499_999_999n && balA1 <= 500_000_000n, `half A back, got ${balA1}`);

      await ctx.removeLiquidity(user, lp - lp / 2n, 1n, 1n);
      const { a, b } = await ctx.vaultBalances();
      assert(a <= 1n && b <= 2n, `vaults drained (dust a=${a} b=${b})`);
      assertEq(await ctx.lpSupply(), 0n, 'lp supply zero');
    });

    await T('Remove liquidity: burning more than supply rejected', async (ctx) => {
      const lp = await ctx.setupPoolWithLiquidity(10_000_000n, 10_000_000n);
      const bal = await ctx.tokenBalance(lp.publicKey, ctx.lpMint);
      await expectError(ctx.removeLiquidity(lp, bal + 1n, 1n, 1n),
        Err.MathUnderflow, 'over-burn');
    });

    // ---------- Swaps (no loans) ----------

    await T('Swap A→B and B→A: amounts match CPMM quote', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const trader = await ctx.newUser(10, 10_000_000n, 50_000_000n);

      await ctx.swap(trader, 10_000_000n, 1n, true);
      const gotB = await ctx.tokenBalance(trader.publicKey, ctx.mintB);
      const expectB = cpmmQuoteOut(10_000_000n, 1_000_000_000n, 4_000_000_000n, 30n);
      assertEq(gotB - 50_000_000n, expectB, 'A→B output equals quote');

      // Program quotes on ACCOUNTED reserves: vault balances minus the
      // protocol-fee share accrued by the first swap (5k A here).
      const { a: va, b: vb } = await ctx.vaultBalances();
      const pool = await ctx.poolState();
      const expectA = cpmmQuoteOut(40_000_000n, vb - pool.protocolFeesB, va - pool.protocolFeesA, 30n);
      await ctx.swap(trader, 40_000_000n, 1n, false);
      const gotA = await ctx.tokenBalance(trader.publicKey, ctx.mintA);
      assertEq(gotA, expectA, 'B→A output equals quote');
    });

    await T('Swap: zero amount and slippage rejected', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const trader = await ctx.newUser(10, 100_000_000n, 0n);
      await expectError(ctx.swap(trader, 0n, 1n, true), Err.ZeroAmount, 'zero in');
      await expectError(ctx.swap(trader, 100_000_000n, 10_000_000_000n, true),
        Err.SlippageExceeded, 'min_out breach');
    });

    await T('Swap: substituted foreign vault rejected', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const other = new Ctx(connection, payer, tokenProgramId);
      await other.setup();
      await other.setupPoolWithLiquidity(1_000_000n, 1_000_000n);
      const trader = await ctx.newUser(10, 100_000_000n, 0n);
      const ix = ctx.ixSwap(trader.publicKey, 1_000_000n, 1n, true, 0, []);
      ix.keys[1].pubkey = other.vaultA; // wrong vault
      await expectError(ctx.send([ix], [trader]), Err.InvalidPool, 'foreign vault');
    });

    await T('Protocol fees: only the program upgrade authority can claim', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const trader = await ctx.newUser(10, 100_000_000n, 0n);
      await ctx.swap(trader, 100_000_000n, 1n, true);
      let pool = await ctx.poolState();
      // fee = 100M * 30bps = 300k; protocol share = 300k * 5/30 = 50k
      assertEq(pool.protocolFeesA, 50_000n, 'protocol fee A accrued');

      const makeAtas = async (owner: PublicKey) => {
        // Idempotent: `stranger` already has ATAs from newUser; the upgrade
        // authority does not — this handles both.
        const ixs: TransactionInstruction[] = [];
        for (const mint of [ctx.mintA, ctx.mintB]) {
          ixs.push(createAssociatedTokenAccountIdempotentInstruction(payer.publicKey,
            ctx.ata(owner, mint), owner, mint, tokenProgramId, ASSOCIATED_TOKEN_PROGRAM_ID));
        }
        await ctx.send(ixs, []);
      };

      // A non-upgrade-authority signer (a stranger — pools grant no authority) is rejected.
      const stranger = await ctx.newUser(10, 0n, 0n);
      await makeAtas(stranger.publicKey);
      await expectError(ctx.claimProtocolFees(stranger),
        Err.InvalidAuthority, 'non-upgrade-authority claim');

      // The program's upgrade authority claims successfully.
      const authority = loadUpgradeAuthority();
      await makeAtas(authority.publicKey);
      await ctx.claimProtocolFees(authority);
      assertEq(await ctx.tokenBalance(authority.publicKey, ctx.mintA), 50_000n,
        'fees received by upgrade authority');
      pool = await ctx.poolState();
      assertEq(pool.protocolFeesA, 0n, 'fee counter reset');
      // Second claim: no-op success.
      await ctx.claimProtocolFees(authority);
    });

    // ---------- Loans ----------

    await T('Open loan (CollateralA): state, band, bitmap, balances', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 0n);
      const { loan, bandId, direction } =
        await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);

      const pool = await ctx.poolState();
      assertEq(pool.openLoans, 1n, 'open_loans');
      assertEq(pool.totalCollateralA, 100_000_000n, 'total_collateral_a');
      assertEq(pool.totalDebtB, 300_000_000n, 'total_debt_b');
      assertEq(direction, DIR_FALL, 'OnFall');
      assert(bitmapIsSet(pool.bandBitmapFall, bandId), 'fall bit set');

      const band = await ctx.bandState(direction, bandId);
      assertEq(band?.count, 1, 'band count');

      const state = await ctx.loanState(loan);
      assertEq(state?.status, 0, 'loan open');
      assertEq(state?.debtPrincipal, 300_000_000n, 'principal');
      // trigger = 300M * 1.1 / 100M = 3.3 B/A
      assertEq(state?.triggerPriceWad, 3_300_000_000_000_000_000n, 'trigger 3.3');

      assertEq(await ctx.tokenBalance(borrower.publicKey, ctx.mintA), 0n, 'collateral gone');
      assertEq(await ctx.tokenBalance(borrower.publicKey, ctx.mintB), 300_000_000n, 'debt received');
    });

    await T('Open loan (CollateralB): mirrored OnRise side', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 0n, 400_000_000n);
      // coll 400M B, debt 75M A: ltv = 75M*4 / 400M = 75% ≤ 80% ✓
      const { bandId, direction } =
        await ctx.openLoan(borrower, COLL_B, 400_000_000n, 75_000_000n);
      assertEq(direction, DIR_RISE, 'OnRise');
      const pool = await ctx.poolState();
      assertEq(pool.totalCollateralB, 400_000_000n, 'total_collateral_b');
      assertEq(pool.totalDebtA, 75_000_000n, 'total_debt_a');
      assert(bitmapIsSet(pool.bandBitmapRise, bandId), 'rise bit set');
      assertEq(await ctx.tokenBalance(borrower.publicKey, ctx.mintA), 75_000_000n, 'debt A received');
    });

    await T('Open loan: LTV, liquidity, and nonce guards', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 200_000_000n, 0n);
      // ltv = 350M / (100M*4) = 87.5% > 80%
      await expectError(ctx.openLoan(borrower, COLL_A, 100_000_000n, 350_000_000n),
        Err.LtvExceedsMax, 'ltv breach');
      // debt above the entire executable B reserve: ltv first? 5B/(200M*4)=625% → LtvExceedsMax fires first;
      // use a big-collateral low-ltv request instead. Need debt > swappable_b (4B)
      // at ltv ≤ 80% → collateral ≥ 1.5625B. Mint more A.
      await mintTo(connection, payer, ctx.mintA, ctx.ata(borrower.publicKey, ctx.mintA),
        payer, 2_000_000_000n, [], { commitment: 'confirmed' }, tokenProgramId);
      await expectError(ctx.openLoan(borrower, COLL_A, 2_000_000_000n, 4_100_000_000n),
        Err.InsufficientExecutableLiquidity, 'debt beyond executable reserve');
      // Stale nonce
      await expectError(
        ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n, { nonceOverride: 99n }),
        Err.InvalidInstruction, 'wrong nonce');
    });

    await T('Repay loan: collateral back, band closed, accounting zeroed', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 50_000_000n);
      const { loan, bandId, direction } =
        await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);

      await ctx.repayLoan(borrower, loan);
      const pool = await ctx.poolState();
      assertEq(pool.openLoans, 0n, 'open_loans');
      assertEq(pool.totalDebtB, 0n, 'debt cleared');
      assertEq(pool.totalCollateralA, 0n, 'collateral cleared');
      assert(!bitmapIsSet(pool.bandBitmapFall, bandId), 'bit cleared');
      assertEq(await ctx.bandState(direction, bandId), null, 'band PDA closed (rent refunded)');
      assertEq(await ctx.loanState(loan), null, 'loan account closed');
      const balA = await ctx.tokenBalance(borrower.publicKey, ctx.mintA);
      assertEq(balA, 100_000_000n, 'collateral returned');
      // Funded 50M extra B to cover interest; repay = principal + accrued ≥ 300M.
      const balB = await ctx.tokenBalance(borrower.publicKey, ctx.mintB);
      assert(balB <= 50_000_000n, 'repaid at least principal');
    });

    await T('Repay loan: wrong borrower rejected', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 0n);
      const { loan } = await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);
      const attacker = await ctx.newUser(10, 0n, 400_000_000n);
      await expectError(ctx.repayLoan(borrower, loan, { signer: attacker }),
        Err.InvalidPool, 'foreign repay');
    });

    // (Live-slot interest test removed: the interest curve is now a fixed
    // program constant with base = 0, so 100%-APR-in-seconds can't be
    // configured. Interest-index math is covered by unit tests in math.rs.)

    // ---------- Liquidation engine ----------

    await T('Swap A→B liquidates CollateralA loan via router context', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 0n);
      const { loan, bandId } = await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);

      // Price 4 → trigger 3.3. 200M A in pushes the price well past it.
      const trader = await ctx.newUser(10, 200_000_000n, 0n);
      await ctx.swap(trader, 200_000_000n, 1n, true);

      const pool = await ctx.poolState();
      assertEq(pool.openLoans, 0n, 'loan liquidated');
      assertEq(pool.totalDebtB, 0n, 'debt written off');
      assertEq(pool.totalCollateralA, 0n, 'collateral seized');
      assert(!bitmapIsSet(pool.bandBitmapFall, bandId), 'band emptied');
      const state = await ctx.loanState(loan);
      assertEq(state?.status, 2, 'tombstoned LIQUIDATED');
      assertEq(state?.collateral, 0n, 'amounts zeroed');
    });

    await T('Swap B→A liquidates CollateralB loan (OnRise)', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 0n, 400_000_000n);
      // trigger = 400M / (75M * 1.1) ≈ 4.848 B/A — fires when price RISES past it.
      const { loan, bandId } = await ctx.openLoan(borrower, COLL_B, 400_000_000n, 75_000_000n);

      // Push price up: need ~25% A drained. 700M B in → price ≈ 5.5.
      const trader = await ctx.newUser(10, 0n, 700_000_000n);
      await ctx.swap(trader, 700_000_000n, 1n, false);

      const pool = await ctx.poolState();
      assertEq(pool.openLoans, 0n, 'loan liquidated');
      assertEq(pool.totalDebtA, 0n, 'A debt written off');
      assertEq(pool.totalCollateralB, 0n, 'B collateral seized');
      assert(!bitmapIsSet(pool.bandBitmapRise, bandId), 'rise band emptied');
      assertEq((await ctx.loanState(loan))?.status, 2, 'tombstoned');
    });

    await T('Liquidated borrower reclaims rent; guards enforced', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 0n);
      const { loan } = await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);

      // Claim while still open → rejected.
      await expectError(ctx.claimLiquidatedRent(borrower, loan),
        Err.LoanNotLiquidatable, 'claim on open loan');

      const trader = await ctx.newUser(10, 200_000_000n, 0n);
      await ctx.swap(trader, 200_000_000n, 1n, true);

      // Wrong borrower → rejected.
      const attacker = await ctx.newUser(10, 0n, 0n);
      await expectError(ctx.claimLiquidatedRent(attacker, loan),
        Err.InvalidAuthority, 'wrong borrower claim');

      const before = await connection.getBalance(borrower.publicKey);
      await ctx.claimLiquidatedRent(borrower, loan);
      const after = await connection.getBalance(borrower.publicKey);
      assert(after > before, 'rent recovered');
      assertEq(await connection.getAccountInfo(loan), null, 'tombstone gone');
    });

    // ---------- Adversarial: completeness proof ----------

    await T('Adversarial: omitting a populated band reverts', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 0n);
      await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);
      const trader = await ctx.newUser(10, 200_000_000n, 0n);
      await expectError(ctx.swapRaw(trader, 200_000_000n, 1n, true, 0, []),
        Err.IncompleteBandWalk, 'empty context');
    });

    await T('Adversarial: partial band membership reverts', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const b1 = await ctx.newUser(10, 50_000_000n, 0n);
      const b2 = await ctx.newUser(10, 50_000_000n, 0n);
      // Two identical-trigger loans share a band (count = 2).
      const l1 = await ctx.openLoan(b1, COLL_A, 50_000_000n, 150_000_000n);
      const l2 = await ctx.openLoan(b2, COLL_A, 50_000_000n, 150_000_000n);
      assertEq(l1.bandId, l2.bandId, 'same band');
      const trader = await ctx.newUser(10, 200_000_000n, 0n);
      // Supply only one of the two members → count mismatch.
      await expectError(
        ctx.swapRaw(trader, 200_000_000n, 1n, true, 0,
          [{ bandId: l1.bandId, loans: [l1.loan] }]),
        Err.IncompleteBandWalk, 'partial band');
    });

    await T('Adversarial: unsorted / duplicated loans revert', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const b1 = await ctx.newUser(10, 50_000_000n, 0n);
      const b2 = await ctx.newUser(10, 50_000_000n, 0n);
      const l1 = await ctx.openLoan(b1, COLL_A, 50_000_000n, 150_000_000n);
      const l2 = await ctx.openLoan(b2, COLL_A, 50_000_000n, 150_000_000n);
      const sorted = [l1.loan, l2.loan]
        .sort((x, y) => Buffer.compare(x.toBuffer(), y.toBuffer()));
      const trader = await ctx.newUser(10, 200_000_000n, 0n);
      await expectError(
        ctx.swapRaw(trader, 200_000_000n, 1n, true, 0,
          [{ bandId: l1.bandId, loans: [sorted[1], sorted[0]] }]),
        Err.InvalidLiquidationContext, 'descending order');
      await expectError(
        ctx.swapRaw(trader, 200_000_000n, 1n, true, 0,
          [{ bandId: l1.bandId, loans: [sorted[0], sorted[0]] }]),
        Err.InvalidLiquidationContext, 'duplicate loan');
    });

    await T('Adversarial: lying about the band boundary reverts', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 0n);
      const { bandId } = await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);
      const trader = await ctx.newUser(10, 200_000_000n, 0n);
      // Claim the price won't fall below band (bandId+1): the bitmap range
      // [bandId+1, MAX] is then empty so no bands are required up front —
      // but the post-cascade boundary recheck catches the lie.
      await expectError(
        ctx.swapRaw(trader, 200_000_000n, 1n, true, bandId + 1, []),
        Err.IncompleteBandWalk, 'boundary lie');
    });

    await T('Regression (M17): swap-emptied band reused by a new loan stays visible', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const b1 = await ctx.newUser(10, 100_000_000n, 0n);
      const { bandId } = await ctx.openLoan(b1, COLL_A, 100_000_000n, 300_000_000n);

      const trader = await ctx.newUser(10, 200_000_000n, 700_000_000n);
      await ctx.swap(trader, 200_000_000n, 1n, true); // liquidates, empties band

      let pool = await ctx.poolState();
      assert(!bitmapIsSet(pool.bandBitmapFall, bandId), 'bit cleared after empty');
      const band = await ctx.bandState(DIR_FALL, bandId);
      assertEq(band?.count, 0, 'band PDA persists with count 0');

      // Pump price back up so a fresh trigger lands in the same band.
      await ctx.swap(trader, 700_000_000n, 1n, false);
      const b2 = await ctx.newUser(10, 100_000_000n, 0n);
      const l2 = await ctx.openLoan(b2, COLL_A, 100_000_000n, 250_000_000n);
      assertEq(l2.bandId, bandId, 'same band reused');

      pool = await ctx.poolState();
      assert(bitmapIsSet(pool.bandBitmapFall, bandId), 'bit re-set on reuse');
      // And a swap omitting it must fail.
      const t2 = await ctx.newUser(10, 200_000_000n, 0n);
      await expectError(ctx.swapRaw(t2, 200_000_000n, 1n, true, 0, []),
        Err.IncompleteBandWalk, 'omit reused band');
    });

    // ---------- Caps ----------

    await T('Cap: 9 triggered loans exceed MAX_LIQ_PER_SWAP; 8 succeed (CU logged)', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 0n);
      let nonce = (await ctx.poolState()).nextLoanNonce;
      const loans: PublicKey[] = [];
      for (let i = 0; i < 9; i++) {
        const r = await ctx.openLoan(borrower, COLL_A, 10_000_000n, 30_000_000n,
          { knownNonce: nonce, commitment: 'processed' });
        loans.push(r.loan);
        nonce += 1n;
      }
      await waitFor(async () => (await ctx.poolState()).openLoans === 9n,
        'nine opens confirmed');
      const pool = await ctx.poolState();
      assertEq(pool.openLoans, 9n, 'nine open loans');

      const trader = await ctx.newUser(10, 400_000_000n, 0n);
      await expectError(ctx.swap(trader, 200_000_000n, 1n, true),
        Err.TooManyLiquidationsRequired, 'nine liquidations needed');

      // Repay one → exactly 8 trigger → swap succeeds at the cap.
      await ctx.repayLoan(borrower, loans[8]);
      const sig = await ctx.swap(trader, 200_000_000n, 1n, true);
      const post = await ctx.poolState();
      assertEq(post.openLoans, 0n, 'all eight liquidated');

      const txInfo = await connection.getTransaction(sig,
        { commitment: 'confirmed', maxSupportedTransactionVersion: 0 });
      const cu = txInfo?.meta?.computeUnitsConsumed ?? 0;
      assert(cu > 0 && cu < 1_400_000, `CU within budget (${cu})`);
      console.log(`    swap with ${MAX_LIQ_PER_SWAP} liquidations: ${cu} CU`);
    });

    await T('Cap: 65th loan in one band rejected with BandFull', async (ctx) => {
      await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 0n);
      let nonce = (await ctx.poolState()).nextLoanNonce;
      let bandId = -1;
      for (let i = 0; i < BAND_MAX_LOANS; i++) {
        const r = await ctx.openLoan(borrower, COLL_A, 1_000_000n, 3_000_000n,
          { knownNonce: nonce, commitment: 'processed' });
        bandId = r.bandId;
        nonce += 1n;
      }
      await waitFor(async () => (await ctx.bandState(DIR_FALL, bandId))?.count === BAND_MAX_LOANS,
        'all 64 opens confirmed');
      const band = await ctx.bandState(DIR_FALL, bandId);
      assertEq(band?.count, BAND_MAX_LOANS, 'band saturated');
      await expectError(
        ctx.openLoan(borrower, COLL_A, 1_000_000n, 3_000_000n, { knownNonce: nonce }),
        Err.BandFull, '65th loan');
    });

    // ---------- Conservation / solvency ----------

    await T('Conservation: post-liquidation pool stays solvent, LPs profit', async (ctx) => {
      const lpUser = await ctx.setupPoolWithLiquidity(1_000_000_000n, 4_000_000_000n);
      const borrower = await ctx.newUser(10, 100_000_000n, 0n);
      await ctx.openLoan(borrower, COLL_A, 100_000_000n, 300_000_000n);
      const trader = await ctx.newUser(10, 200_000_000n, 0n);
      await ctx.swap(trader, 200_000_000n, 1n, true);

      // No open loans → LP can withdraw everything; only protocol fees + dust stay.
      const lpBal = await ctx.tokenBalance(lpUser.publicKey, ctx.lpMint);
      await ctx.removeLiquidity(lpUser, lpBal, 1n, 1n);
      const pool = await ctx.poolState();
      const { a, b } = await ctx.vaultBalances();
      assert(a >= pool.protocolFeesA && a - pool.protocolFeesA <= 2n,
        `vault A = fees + dust (a=${a} fees=${pool.protocolFeesA})`);
      assert(b >= pool.protocolFeesB && b - pool.protocolFeesB <= 2n,
        `vault B = fees + dust (b=${b} fees=${pool.protocolFeesB})`);

      // The liquidation seized 100M A collateral against 300M B forgiven debt:
      // LP withdrew (1B + 200M swap-in + 100M collateral − swap-out − ...) —
      // simply assert the LP's A-side take exceeds the original deposit.
      const lpA = await ctx.tokenBalance(lpUser.publicKey, ctx.mintA);
      assert(lpA > 1_000_000_000n, `LP gained A from seizure (${lpA})`);
    });

    await T('Heavy borrow blocks LP withdrawal until repaid', async (ctx) => {
      const lpUser = await ctx.setupPoolWithLiquidity(1_000_000_000n, 1_000_000_000n);
      const borrower = await ctx.newUser(10, 2_000_000_000n, 100_000_000n);
      // Borrow 70% of B against A (ltv 70/87.5... coll 1.25B A debt 700M B → ltv=700/1250/1(price1)=56%)
      const { loan } = await ctx.openLoan(borrower, COLL_A, 1_250_000_000n, 700_000_000n);
      const lpBal = await ctx.tokenBalance(lpUser.publicKey, ctx.lpMint);
      await expectError(ctx.removeLiquidity(lpUser, lpBal, 1n, 1n),
        Err.InsufficientExecutableLiquidity, 'full exit while lent out');
      // Partial exit within executable reserve still works.
      await ctx.removeLiquidity(lpUser, lpBal / 10n, 1n, 1n);
      // After repay, full exit works.
      await ctx.repayLoan(borrower, loan);
      const rest = await ctx.tokenBalance(lpUser.publicKey, ctx.lpMint);
      await ctx.removeLiquidity(lpUser, rest, 1n, 1n);
      assertEq(await ctx.lpSupply(), 0n, 'fully exited');
    });

    // (Admin test removed: pools are immutable and authority-less — there is
    // no UpdatePoolSettings or TransferAuthority. Fee-claim authorization is
    // covered by the "Protocol fees" test above.)
  }

  console.log(`\n=== Results: ${passed} passed, ${failed} failed ===`);
  process.exit(failed > 0 ? 1 : 0);
}

// Only auto-run when invoked directly (`ts-node test_liquidity.ts`). When
// imported by test_adversarial.ts to reuse the harness, this stays dormant.
if (require.main === module) {
  runTests().catch((e) => {
    console.error(e);
    process.exit(1);
  });
}

// Shared harness surface for sibling test files (e.g. test_adversarial.ts).
export {
  Ctx, Err, Ix, DEFAULT_PARAMS,
  COLL_A, COLL_B, DIR_FALL, DIR_RISE,
  PROGRAM_ID, POOL_SEED, VAULT_A_SEED, VAULT_B_SEED, LP_MINT_SEED, LOAN_SEED, BAND_SEED,
  LOAN_LEN, BAND_LEN, POOL_LEN, LOAN_OFF, LOAN_DISCRIMINATOR,
  WAD, BPS, BAND_MAX_LOANS, MAX_LIQ_PER_SWAP,
  recomputeTrigger, bandIdForTrigger, bitmapIsSet, cpmmQuoteOut, isqrt, sleep, waitFor,
  decodePool, decodeLoan, decodeBand,
  assert, assertEq, expectError, customErrorCode,
};
export type { PoolParams, PoolState, LoanState, BandState };
