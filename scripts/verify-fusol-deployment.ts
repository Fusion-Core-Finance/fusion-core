/**
 * fuSOL deployment/config verifier — the FAIL-CLOSED launch gate (FUSOL-08; spec §4.3, §17.4, §19).
 *
 * An independent, READ-ONLY tool (a plain Connection — no wallet, no signing, no mutation) that
 * checks the LIVE on-chain deployment against the expected immutable configuration. Run it — and it
 * must PASS — twice:
 *
 *   1. BEFORE renouncing the program upgrade authorities  (config: expectUpgradeAuthority = the
 *      guarded authority pubkey per program; --phase pre-seal)
 *   2. AFTER renouncing them                               (config: expectUpgradeAuthority = "none"
 *      for the renounced programs; --phase sealed) — proves the authorities are actually None.
 *
 * SECURITY GATE DISCIPLINE: a verifier that silently PASSES a bad deployment is worse than none, so
 * EVERY check is fail-closed. A missing / unreadable / unparseable account, an RPC error, a missing
 * pinned hash, or ANY mismatch is a FAILURE — never a skip. The process exits 1 if any check FAILS
 * or could not run.
 *
 * The checks never trust the config's addresses blindly: every PDA is re-derived from the SDK
 * (sdk/src/stake-pool.ts) + the on-chain seeds, and a config that lies about a PDA fails. The full
 * authority graph is then cross-read from the live StakePool + ControllerConfig accounts.
 *
 * BYTE LAYOUTS are cross-referenced to their sources so the offsets can be audited against them:
 *   - SPL Mint / Token Account            spl_token::state (mirrors scripts/bootstrap-fusol.ts)
 *   - StakeStateV2 (bincode)              solana stake program (mirrors bootstrap-fusol.ts)
 *   - StakePool / ValidatorList (borsh)   vendor/spl-stake-pool/program/src/state.rs
 *   - ControllerConfig (anchor/borsh)     programs/fusion-stake-controller/src/state/controller_config.rs
 *   - MarketOracle / Market (anchor)      programs/fusd-core/src/state/{market_oracle,market}.rs
 *   - Program / ProgramData (bincode)     BPFLoaderUpgradeable (4-byte state tag + 8-byte slot +
 *                                         1-byte Option tag + 32-byte authority)
 *
 * USAGE
 *   npx ts-node scripts/verify-fusol-deployment.ts [config.json] [--phase pre-seal|sealed] [--json] [--commitment finalized|confirmed]
 *   Reads at `finalized` for a sealed (post-renounce) run — the gate certifies an irreversible
 *   step, so a confirmed-but-not-finalized read that a fork could roll back must not flash PASS.
 *   Config path: the positional arg, --config <path>, or the FUSOL_VERIFY_CONFIG env var.
 *
 * The pure check functions (info bytes + expected -> string|null error) are exported and unit-tested
 * in keepers/verify-fusol-deployment.spec.ts — the good-fixture-passes / each-corruption-fails matrix
 * is the whole point of a security gate.
 */
import { createHash } from "crypto";
import * as fs from "fs";
import { PublicKey, Connection, Commitment } from "@solana/web3.js";
import {
  CONTROLLER_PROGRAM_ID,
  STAKE_POOL_FORK_ID,
  controllerConfig as deriveControllerConfig,
  poolAuthority as derivePoolAuthority,
  depositAuthority as deriveDepositAuthority,
  maintenanceAuthority as deriveMaintenanceAuthority,
  poolWithdrawAuthority as derivePoolWithdrawAuthority,
} from "../sdk/src/stake-pool";

// ── well-known program ids + constants (pinned; cross-referenced to their on-chain sources) ──────
export const TOKEN_PROGRAM = new PublicKey(
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
); // legacy SPL
export const BPF_LOADER_UPGRADEABLE = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111",
);
export const STAKE_PROGRAM = new PublicKey(
  "Stake11111111111111111111111111111111111111",
);
export const ZERO_KEY = new PublicKey(new Uint8Array(32)); // Pubkey::default() — the "None"/unset sentinel

// Account sizes pinned to the vendored fork / SPL Token / the Rust #[account] SPACE consts.
export const MINT_SPACE = 82; // spl_token::state::Mint::LEN
export const TOKEN_ACCOUNT_SPACE = 165; // spl_token::state::Account::LEN
export const STAKE_ACCOUNT_SPACE = 200; // size_of::<StakeStateV2>()
export const STAKE_POOL_SPACE = 611; // get_packed_len::<StakePool>() MAX (all Options Some, FutureEpoch Two)
export const MAX_VALIDATORS = 1024;
export const VALIDATOR_LIST_SPACE = 9 + MAX_VALIDATORS * 73; // header(5) + vec len(4) + MAX * ValidatorStakeInfo::LEN(73)
export const CONTROLLER_CONFIG_SPACE = 366; // ControllerConfig::SPACE
export const MARKET_ORACLE_SPACE = 335; // MarketOracle::SPACE
export const MARKET_SPACE = 510; // Market::SPACE
export const UPGRADEABLE_PROGRAMDATA_HEADER = 45; // 4 (state tag) + 8 (slot) + 1 (Option tag) + 32 (authority)

// programs/fusd-core/src/constants.rs — the shared Pyth SOL/USD feed id (the canonical-primary leg).
export const PYTH_SOL_USD_FEED_ID = Buffer.from([
  0xef, 0x0d, 0x8b, 0x6f, 0xcd, 0x01, 0x04, 0xe3, 0xe7, 0x50, 0x96, 0x91, 0x2f,
  0xc8, 0xe1, 0xe4, 0x32, 0x89, 0x3d, 0xa4, 0xf1, 0x8f, 0xae, 0xda, 0xac, 0xca,
  0x7e, 0x58, 0x75, 0xda, 0x62, 0x0f,
]);
export const MAX_LIQUIDITY_HAIRCUT_BPS = 2_000; // constants::MAX_LIQUIDITY_HAIRCUT_BPS
// constants::LIQ_INFRA_* — borrow requires `flags & LIQ_INFRA_READY_MASK == LIQ_INFRA_READY_MASK`.
export const LIQ_INFRA_REACTOR_POOL = 1 << 1;
export const LIQ_INFRA_INSURANCE_BUFFER = 1 << 2;
export const LIQ_INFRA_READY_MASK =
  LIQ_INFRA_REACTOR_POOL | LIQ_INFRA_INSURANCE_BUFFER;

// Fixed pool fee schedule (initialize_pool; no fee setter exists in the controller binary).
const EPOCH_FEE = { num: 1n, denom: 100n }; // 1% of positive epoch rewards
const DEPOSIT_FEE = { num: 5n, denom: 10_000n }; // 5 bps on each deposit flavor
const WITHDRAWAL_FEE = { num: 5n, denom: 10_000n }; // 5 bps on each withdrawal flavor

type Pk = PublicKey;

/** The minimal account "info" the pure checks read. Fixtures build these directly (no live RPC). */
export interface Acct {
  owner: Pk;
  data: Buffer;
  executable?: boolean;
}

const b58 = (k: Pk) => k.toBase58();

// ── anchor discriminator (sha256("account:<Name>")[..8]) ─────────────────────────────────────────
export function anchorDiscriminator(name: string): Buffer {
  return createHash("sha256").update(`account:${name}`).digest().subarray(0, 8);
}

// ── a bounds-checked, fail-closed byte reader (borsh-style sequential parse) ──────────────────────
export interface Fee {
  denominator: bigint;
  numerator: bigint;
}
export interface FutureEpochVal {
  tag: number; // 0 = None, 1 = One, 2 = Two
  fee: Fee | null;
}

class Reader {
  constructor(
    private data: Buffer,
    public off = 0,
  ) {}
  private need(n: number, what: string): void {
    if (this.off + n > this.data.length)
      throw new Error(
        `truncated reading ${what} at offset ${this.off} (need ${n}, have ${this.data.length - this.off})`,
      );
  }
  u8(what: string): number {
    this.need(1, what);
    return this.data[this.off++];
  }
  u32(what: string): number {
    this.need(4, what);
    const v = this.data.readUInt32LE(this.off);
    this.off += 4;
    return v;
  }
  u64(what: string): bigint {
    this.need(8, what);
    const v = this.data.readBigUInt64LE(this.off);
    this.off += 8;
    return v;
  }
  pubkey(what: string): Pk {
    this.need(32, what);
    const v = new PublicKey(this.data.subarray(this.off, this.off + 32));
    this.off += 32;
    return v;
  }
  skip(n: number, what: string): void {
    this.need(n, what);
    this.off += n;
  }
  fee(what: string): Fee {
    // vendor state.rs Fee { denominator: u64, numerator: u64 } — denominator FIRST.
    return {
      denominator: this.u64(`${what}.denominator`),
      numerator: this.u64(`${what}.numerator`),
    };
  }
  futureEpoch(what: string): FutureEpochVal {
    const tag = this.u8(`${what}.tag`);
    if (tag === 0) return { tag, fee: null };
    if (tag === 1 || tag === 2) return { tag, fee: this.fee(`${what}.value`) };
    throw new Error(`invalid FutureEpoch tag ${tag} for ${what}`);
  }
  optionPubkey(what: string): Pk | null {
    const tag = this.u8(`${what}.tag`);
    if (tag === 0) return null;
    if (tag === 1) return this.pubkey(`${what}.value`);
    throw new Error(`invalid Option tag ${tag} for ${what}`);
  }
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CHECK 1 — PROGRAMS EXECUTABLE + UPGRADE AUTHORITY (the headline immutability check)
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/** Parse a BPFLoaderUpgradeable Program account -> its ProgramData address (state tag u32 == 2). */
export function parseProgramDataAddress(data: Buffer): Pk {
  if (data.length < 36)
    throw new Error(`program account too short (${data.length} < 36)`);
  const tag = data.readUInt32LE(0);
  if (tag !== 2)
    throw new Error(`not a Program account (state tag ${tag}, expected 2)`);
  return new PublicKey(data.subarray(4, 36));
}

export interface UpgradeAuthorityInfo {
  slot: bigint;
  authority: Pk | null;
}
/** Parse a ProgramData account header: state tag u32 == 3, slot u64, then Option<Pubkey> authority. */
export function parseUpgradeAuthority(data: Buffer): UpgradeAuthorityInfo {
  if (data.length < 13)
    throw new Error(`programdata account too short (${data.length} < 13)`);
  const tag = data.readUInt32LE(0);
  if (tag !== 3)
    throw new Error(`not a ProgramData account (state tag ${tag}, expected 3)`);
  const slot = data.readBigUInt64LE(4);
  const optTag = data[12];
  if (optTag === 0) return { slot, authority: null };
  if (optTag === 1) {
    if (data.length < 45)
      throw new Error(
        `programdata authority Some but account too short (${data.length} < 45)`,
      );
    return { slot, authority: new PublicKey(data.subarray(13, 45)) };
  }
  throw new Error(`invalid upgrade-authority Option tag ${optTag}`);
}

/**
 * The full immutability check for one program: executable + owned by the upgradeable loader, its
 * embedded ProgramData address matches the derived PDA, and the ProgramData's upgrade authority
 * equals `expect` (None when "none"). `null` == pass.
 */
export function checkProgramImmutability(
  programAcct: Acct | null,
  programDataAcct: Acct | null,
  derivedProgramData: Pk,
  expect: "none" | Pk,
): string | null {
  if (!programAcct) return "program account is missing/unreadable";
  if (!programAcct.executable) return "program account is not executable";
  if (!programAcct.owner.equals(BPF_LOADER_UPGRADEABLE))
    return `program owner ${b58(programAcct.owner)} is not the BPF upgradeable loader`;
  let embedded: Pk;
  try {
    embedded = parseProgramDataAddress(programAcct.data);
  } catch (e: any) {
    return `program account parse failed: ${e.message}`;
  }
  if (!embedded.equals(derivedProgramData))
    return `program's programdata address ${b58(embedded)} != derived ProgramData PDA ${b58(derivedProgramData)}`;
  if (!programDataAcct) return "programdata account is missing/unreadable";
  if (!programDataAcct.owner.equals(BPF_LOADER_UPGRADEABLE))
    return `programdata owner ${b58(programDataAcct.owner)} is not the BPF upgradeable loader`;
  let ua: UpgradeAuthorityInfo;
  try {
    ua = parseUpgradeAuthority(programDataAcct.data);
  } catch (e: any) {
    return `programdata parse failed: ${e.message}`;
  }
  if (expect === "none") {
    if (ua.authority !== null)
      return `upgrade authority is ${b58(ua.authority)} (expected None — the upgrade authority is NOT renounced)`;
  } else {
    if (ua.authority === null)
      return `upgrade authority is None (expected ${b58(expect)})`;
    if (!ua.authority.equals(expect))
      return `upgrade authority is ${b58(ua.authority)} (expected ${b58(expect)})`;
  }
  return null;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CHECK 2 — PROGRAM ELF HASH (+ security.txt, informational)
// ═══════════════════════════════════════════════════════════════════════════════════════════════

export interface ElfHashResult {
  status: "PASS" | "FAIL" | "NOT_VERIFIED";
  detail: string;
  actualHex?: string;
}
/** sha256 the on-chain ProgramData ELF (bytes past the 45-byte metadata header) vs the pinned hash. */
export function checkProgramElfHash(
  programDataAcct: Acct | null,
  expectedHex?: string,
): ElfHashResult {
  if (!programDataAcct)
    return {
      status: "FAIL",
      detail: "programdata account is missing/unreadable",
    };
  if (programDataAcct.data.length <= UPGRADEABLE_PROGRAMDATA_HEADER)
    return {
      status: "FAIL",
      detail: `programdata too short (${programDataAcct.data.length}) to contain an ELF`,
    };
  const elf = programDataAcct.data.subarray(UPGRADEABLE_PROGRAMDATA_HEADER);
  const actualHex = createHash("sha256").update(elf).digest("hex");
  if (!expectedHex)
    return {
      status: "NOT_VERIFIED",
      detail: `on-chain ELF sha256 ${actualHex} — NO expected hash pinned in config (a launch gate cannot PASS unverified)`,
      actualHex,
    };
  const want = expectedHex.replace(/^0x/, "").toLowerCase();
  if (actualHex !== want)
    return {
      status: "FAIL",
      detail: `ELF sha256 ${actualHex} != expected ${want}`,
      actualHex,
    };
  return { status: "PASS", detail: `ELF sha256 ${actualHex}`, actualHex };
}

/** Extract the on-chain security.txt (informational only — never affects pass/fail). */
export function extractSecurityTxt(
  programDataData: Buffer,
): Record<string, string> | null {
  const begin = Buffer.from("=======BEGIN SECURITY.TXT V1=======\0");
  const end = Buffer.from("=======END SECURITY.TXT V1=======\0");
  const start = programDataData.indexOf(begin);
  if (start < 0) return null;
  const stop = programDataData.indexOf(end, start + begin.length);
  if (stop < 0) return null;
  const body = programDataData.subarray(start + begin.length, stop);
  const parts = body.toString("utf8").split("\0");
  const out: Record<string, string> = {};
  for (let i = 0; i + 1 < parts.length; i += 2) {
    if (parts[i]) out[parts[i]] = parts[i + 1];
  }
  return out;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CHECK 3 — fuSOL MINT
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/** SPL Mint: mint_authority COption (tag u32 @0, key @4), supply u64 @36, decimals u8 @44,
 * is_initialized u8 @45, freeze_authority COption (tag u32 @46, key @50). Mirrors bootstrap verifyMint. */
export function checkFusolMint(
  acct: Acct | null,
  expectedMintAuthority: Pk,
): string | null {
  if (!acct) return "mint account is missing/unreadable";
  if (!acct.owner.equals(TOKEN_PROGRAM))
    return `owner ${b58(acct.owner)} is not the SPL Token program`;
  if (acct.data.length !== MINT_SPACE)
    return `size ${acct.data.length} != ${MINT_SPACE}`;
  const d = acct.data;
  if (d[45] !== 1) return "mint is not initialized";
  if (d[44] !== 9) return `decimals ${d[44]} != 9`;
  if (
    d.readUInt32LE(0) !== 1 ||
    !new PublicKey(d.subarray(4, 36)).equals(expectedMintAuthority)
  )
    return `mint authority is not the pool withdraw-authority PDA ${b58(expectedMintAuthority)}`;
  if (d.readUInt32LE(46) !== 0) return "freeze authority is set (must be None)";
  if (d.readBigUInt64LE(36) <= 0n)
    return "supply is 0 (expected the genesis mint to the maintenance vault)";
  return null;
}
export function readMintSupply(mintData: Buffer): bigint {
  return mintData.readBigUInt64LE(36);
}
/** Cross-invariant: the mint supply equals the pool's pool_token_supply (they are the same tokens). */
export function checkMintSupplyMatchesPool(
  mintData: Buffer,
  poolTokenSupply: bigint,
): string | null {
  const supply = readMintSupply(mintData);
  if (supply !== poolTokenSupply)
    return `mint supply ${supply} != pool_token_supply ${poolTokenSupply}`;
  return null;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CHECK 4 — STAKEPOOL (full authority graph + fee schedule via a borsh tail walk)
// ═══════════════════════════════════════════════════════════════════════════════════════════════

export interface StakePoolParsed {
  accountType: number;
  manager: Pk;
  staker: Pk;
  stakeDepositAuthority: Pk;
  validatorList: Pk;
  reserveStake: Pk;
  poolMint: Pk;
  managerFeeAccount: Pk;
  tokenProgramId: Pk;
  totalLamports: bigint;
  poolTokenSupply: bigint;
  epochFee: Fee;
  nextEpochFee: FutureEpochVal;
  stakeDepositFee: Fee;
  stakeWithdrawalFee: Fee;
  nextStakeWithdrawalFee: FutureEpochVal;
  stakeReferralFee: number;
  solDepositAuthority: Pk | null;
  solDepositFee: Fee;
  solReferralFee: number;
  solWithdrawAuthority: Pk | null;
  solWithdrawalFee: Fee;
  nextSolWithdrawalFee: FutureEpochVal;
}

/** Parse the borsh StakePool (head + variable tail). Throws (fail-closed) on truncation / bad tags.
 * Layout: vendor/spl-stake-pool/program/src/state.rs. */
export function parseStakePool(data: Buffer): StakePoolParsed {
  const r = new Reader(data);
  const accountType = r.u8("account_type");
  const manager = r.pubkey("manager");
  const staker = r.pubkey("staker");
  const stakeDepositAuthority = r.pubkey("stake_deposit_authority");
  r.skip(1, "stake_withdraw_bump_seed");
  const validatorList = r.pubkey("validator_list");
  const reserveStake = r.pubkey("reserve_stake");
  const poolMint = r.pubkey("pool_mint");
  const managerFeeAccount = r.pubkey("manager_fee_account");
  const tokenProgramId = r.pubkey("token_program_id");
  const totalLamports = r.u64("total_lamports");
  const poolTokenSupply = r.u64("pool_token_supply");
  r.skip(8, "last_update_epoch");
  r.skip(48, "lockup"); // Lockup { unix_timestamp i64, epoch u64, custodian Pubkey }
  const epochFee = r.fee("epoch_fee"); // @330
  const nextEpochFee = r.futureEpoch("next_epoch_fee"); // @346 — the tail begins
  r.optionPubkey("preferred_deposit_validator"); // permissionlessly settable; not asserted
  r.optionPubkey("preferred_withdraw_validator"); // set by the controller's finalize_plan CPI; not asserted
  const stakeDepositFee = r.fee("stake_deposit_fee");
  const stakeWithdrawalFee = r.fee("stake_withdrawal_fee");
  const nextStakeWithdrawalFee = r.futureEpoch("next_stake_withdrawal_fee");
  const stakeReferralFee = r.u8("stake_referral_fee");
  const solDepositAuthority = r.optionPubkey("sol_deposit_authority");
  const solDepositFee = r.fee("sol_deposit_fee");
  const solReferralFee = r.u8("sol_referral_fee");
  const solWithdrawAuthority = r.optionPubkey("sol_withdraw_authority");
  const solWithdrawalFee = r.fee("sol_withdrawal_fee");
  const nextSolWithdrawalFee = r.futureEpoch("next_sol_withdrawal_fee");
  r.u64("last_epoch_pool_token_supply");
  r.u64("last_epoch_total_lamports");
  return {
    accountType,
    manager,
    staker,
    stakeDepositAuthority,
    validatorList,
    reserveStake,
    poolMint,
    managerFeeAccount,
    tokenProgramId,
    totalLamports,
    poolTokenSupply,
    epochFee,
    nextEpochFee,
    stakeDepositFee,
    stakeWithdrawalFee,
    nextStakeWithdrawalFee,
    stakeReferralFee,
    solDepositAuthority,
    solDepositFee,
    solReferralFee,
    solWithdrawAuthority,
    solWithdrawalFee,
    nextSolWithdrawalFee,
  };
}

export interface StakePoolExpect {
  poolAuthority: Pk; // manager == staker
  depositAuthority: Pk; // stake_deposit_authority == sol_deposit_authority
  validatorList: Pk;
  reserveStake: Pk;
  fusolMint: Pk;
  maintenanceVault: Pk;
}

function feeErr(
  fee: Fee,
  exp: { num: bigint; denom: bigint },
  label: string,
): string | null {
  if (fee.numerator !== exp.num || fee.denominator !== exp.denom)
    return `${label} is ${fee.numerator}/${fee.denominator} (expected ${exp.num}/${exp.denom})`;
  return null;
}

/** Full StakePool check: owner = fork, account_type = StakePool, the entire authority graph, and the
 * fee schedule (walked out of the borsh tail). `null` == pass. */
export function checkStakePool(
  acct: Acct | null,
  exp: StakePoolExpect,
): string | null {
  if (!acct) return "stake pool account is missing/unreadable";
  if (!acct.owner.equals(STAKE_POOL_FORK_ID))
    return `owner ${b58(acct.owner)} is not the stake-pool fork`;
  if (acct.data.length !== STAKE_POOL_SPACE)
    return `size ${acct.data.length} != ${STAKE_POOL_SPACE}`;
  let p: StakePoolParsed;
  try {
    p = parseStakePool(acct.data);
  } catch (e: any) {
    return `stake pool parse failed: ${e.message}`;
  }
  if (p.accountType !== 1)
    return `account_type ${p.accountType} != StakePool (1)`;
  const graph: [string, Pk, Pk][] = [
    ["manager", p.manager, exp.poolAuthority],
    ["staker", p.staker, exp.poolAuthority],
    ["stake_deposit_authority", p.stakeDepositAuthority, exp.depositAuthority],
    ["validator_list", p.validatorList, exp.validatorList],
    ["reserve_stake", p.reserveStake, exp.reserveStake],
    ["pool_mint", p.poolMint, exp.fusolMint],
    ["manager_fee_account", p.managerFeeAccount, exp.maintenanceVault],
    ["token_program_id", p.tokenProgramId, TOKEN_PROGRAM],
  ];
  for (const [name, got, want] of graph)
    if (!got.equals(want))
      return `${name} is ${b58(got)}, expected ${b58(want)}`;
  // Fee schedule (fixed at initialize_pool; the controller binary has no fee setter).
  const feeChecks: [Fee, { num: bigint; denom: bigint }, string][] = [
    [p.epochFee, EPOCH_FEE, "epoch_fee"],
    [p.stakeDepositFee, DEPOSIT_FEE, "stake_deposit_fee"],
    [p.solDepositFee, DEPOSIT_FEE, "sol_deposit_fee"],
    [p.stakeWithdrawalFee, WITHDRAWAL_FEE, "stake_withdrawal_fee"],
    [p.solWithdrawalFee, WITHDRAWAL_FEE, "sol_withdrawal_fee"],
  ];
  for (const [fee, want, label] of feeChecks) {
    const e = feeErr(fee, want, label);
    if (e) return e;
  }
  if (p.stakeReferralFee !== 0)
    return `stake_referral_fee is ${p.stakeReferralFee} (expected 0)`;
  if (p.solReferralFee !== 0)
    return `sol_referral_fee is ${p.solReferralFee} (expected 0)`;
  // No scheduled fee change may be pending (fee schedule is immutable).
  if (p.nextEpochFee.tag !== 0)
    return "next_epoch_fee is set (a scheduled fee change must not exist)";
  if (p.nextStakeWithdrawalFee.tag !== 0)
    return "next_stake_withdrawal_fee is set (must be None)";
  if (p.nextSolWithdrawalFee.tag !== 0)
    return "next_sol_withdrawal_fee is set (must be None)";
  // Deposit authorities gate deposits through the controller; the SOL exit is never gated.
  if (
    p.solDepositAuthority === null ||
    !p.solDepositAuthority.equals(exp.depositAuthority)
  )
    return `sol_deposit_authority is ${p.solDepositAuthority ? b58(p.solDepositAuthority) : "None"}, expected the deposit-authority PDA ${b58(exp.depositAuthority)}`;
  if (p.solWithdrawAuthority !== null)
    return `sol_withdraw_authority is ${b58(p.solWithdrawAuthority)} (must be None — the SOL exit is never gated)`;
  if (p.totalLamports <= 0n)
    return "total_lamports is 0 (expected the genesis reserve funding)";
  if (p.poolTokenSupply <= 0n)
    return "pool_token_supply is 0 (expected the genesis fuSOL mint)";
  return null;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CHECK 5 — VALIDATOR LIST
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/** ValidatorList::calculate_max_validators — (len - header(5) - vec_len(4)) / ValidatorStakeInfo::LEN(73). */
export function calculateMaxValidators(bufferLength: number): number {
  return Math.floor((bufferLength - 9) / 73);
}
/** owner = fork, account_type == ValidatorList (2), stored max == capacity-from-size == MAX_VALIDATORS. */
export function checkValidatorList(acct: Acct | null): string | null {
  if (!acct) return "validator list account is missing/unreadable";
  if (!acct.owner.equals(STAKE_POOL_FORK_ID))
    return `owner ${b58(acct.owner)} is not the stake-pool fork`;
  if (acct.data.length < 9)
    return `size ${acct.data.length} is shorter than the 9-byte header`;
  if (acct.data[0] !== 2)
    return `account_type ${acct.data[0]} != ValidatorList (2)`;
  const storedMax = acct.data.readUInt32LE(1);
  if (storedMax !== MAX_VALIDATORS)
    return `max_validators ${storedMax} != ${MAX_VALIDATORS}`;
  const capacity = calculateMaxValidators(acct.data.length);
  if (capacity !== MAX_VALIDATORS)
    return `account size ${acct.data.length} holds ${capacity} validators, expected ${MAX_VALIDATORS}`;
  return null;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CHECK 6 — MAINTENANCE VAULT
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/** SPL Token Account: mint @0, owner @32, state u8 @108, delegate COption @72, close_authority COption
 * @129. Mirrors bootstrap verifyVault. */
export function checkMaintenanceVault(
  acct: Acct | null,
  fusolMint: Pk,
  maintenanceAuthority: Pk,
): string | null {
  if (!acct) return "maintenance vault account is missing/unreadable";
  if (!acct.owner.equals(TOKEN_PROGRAM))
    return `owner ${b58(acct.owner)} is not the SPL Token program`;
  if (acct.data.length !== TOKEN_ACCOUNT_SPACE)
    return `size ${acct.data.length} != ${TOKEN_ACCOUNT_SPACE}`;
  const d = acct.data;
  if (d[108] !== 1) return "token account is not in the Initialized state";
  if (!new PublicKey(d.subarray(0, 32)).equals(fusolMint))
    return "vault mint is not the fuSOL mint";
  if (!new PublicKey(d.subarray(32, 64)).equals(maintenanceAuthority))
    return `token authority is not the maintenance PDA ${b58(maintenanceAuthority)}`;
  if (d.readUInt32LE(72) !== 0) return "a delegate is set (must be None)";
  if (d.readUInt32LE(129) !== 0)
    return "a close authority is set (must be None)";
  return null;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CHECK 7 — RESERVE STAKE
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/** StakeStateV2 (bincode): discriminant u32 @0 (1=Initialized, 2=Stake), Meta.authorized.staker @12,
 * withdrawer @44, Meta.lockup @76..124. Mirrors bootstrap verifyReserve + adds the no-lockup check. */
export function checkReserveStake(
  acct: Acct | null,
  poolWithdrawAuthority: Pk,
): string | null {
  if (!acct) return "reserve stake account is missing/unreadable";
  if (!acct.owner.equals(STAKE_PROGRAM))
    return `owner ${b58(acct.owner)} is not the stake program`;
  if (acct.data.length !== STAKE_ACCOUNT_SPACE)
    return `size ${acct.data.length} != ${STAKE_ACCOUNT_SPACE}`;
  const d = acct.data;
  const tag = d.readUInt32LE(0);
  if (tag !== 1 && tag !== 2)
    return `stake state ${tag} is neither Initialized (1) nor Stake (2)`;
  if (!new PublicKey(d.subarray(12, 44)).equals(poolWithdrawAuthority))
    return `staker is not the pool withdraw-authority PDA ${b58(poolWithdrawAuthority)}`;
  if (!new PublicKey(d.subarray(44, 76)).equals(poolWithdrawAuthority))
    return `withdrawer is not the pool withdraw-authority PDA ${b58(poolWithdrawAuthority)}`;
  // Lockup { unix_timestamp i64 @76, epoch u64 @84, custodian Pubkey @92 } — all zero == no lockup.
  for (let i = 76; i < 124; i++)
    if (d[i] !== 0) return "reserve stake has a lockup set (must be none)";
  return null;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CHECK 8 — CONTROLLER CONFIG
// ═══════════════════════════════════════════════════════════════════════════════════════════════

export interface ControllerConfigExpect {
  stakePoolProgram: Pk; // == fork id
  stakePool: Pk;
  validatorList: Pk;
  reserveStake: Pk;
  fusolMint: Pk;
  poolWithdrawAuthority: Pk;
  maintenanceVault: Pk;
  fusdCoreProgram: Pk;
  bump: number;
  poolAuthorityBump: number;
  depositAuthorityBump: number;
  maintenanceAuthorityBump: number;
}
/** ControllerConfig (anchor/borsh). Offsets from controller_config.rs, +8 anchor discriminator:
 * version u8 @8, sealed bool @9, then 9 Pubkeys @10.., 4 bumps @298.., _reserved @302. */
export function checkControllerConfig(
  acct: Acct | null,
  exp: ControllerConfigExpect,
): string | null {
  if (!acct) return "controller config account is missing/unreadable";
  if (!acct.owner.equals(CONTROLLER_PROGRAM_ID))
    return `owner ${b58(acct.owner)} is not the controller program`;
  if (acct.data.length !== CONTROLLER_CONFIG_SPACE)
    return `size ${acct.data.length} != ${CONTROLLER_CONFIG_SPACE}`;
  if (!acct.data.subarray(0, 8).equals(anchorDiscriminator("ControllerConfig")))
    return "wrong anchor discriminator (not a ControllerConfig account)";
  const d = acct.data;
  if (d[8] !== 1) return `version ${d[8]} != 1`;
  if (d[9] !== 1)
    return "controller is not sealed (initialize_pool has not run)";
  const addrs: [string, number, Pk][] = [
    ["stake_pool_program", 10, exp.stakePoolProgram],
    ["stake_pool", 42, exp.stakePool],
    ["validator_list", 74, exp.validatorList],
    ["reserve_stake", 106, exp.reserveStake],
    ["fusol_mint", 138, exp.fusolMint],
    ["pool_withdraw_authority", 170, exp.poolWithdrawAuthority],
    ["maintenance_vault", 202, exp.maintenanceVault],
    ["fusd_core_program", 234, exp.fusdCoreProgram],
    ["fusol_collateral_mint", 266, exp.fusolMint], // MUST equal fusol_mint (init-enforced)
  ];
  for (const [name, off, want] of addrs) {
    const got = new PublicKey(d.subarray(off, off + 32));
    if (!got.equals(want))
      return `${name} is ${b58(got)}, expected ${b58(want)}`;
  }
  const bumps: [string, number, number][] = [
    ["bump", 298, exp.bump],
    ["pool_authority_bump", 299, exp.poolAuthorityBump],
    ["deposit_authority_bump", 300, exp.depositAuthorityBump],
    ["maintenance_authority_bump", 301, exp.maintenanceAuthorityBump],
  ];
  for (const [name, off, want] of bumps)
    if (d[off] !== want)
      return `${name} is ${d[off]}, expected the canonical bump ${want}`;
  return null;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CHECK 9 — fusd-core fuSOL MARKET + ORACLE
// ═══════════════════════════════════════════════════════════════════════════════════════════════

export interface MarketOracleExpect {
  fusdCoreProgram: Pk;
  fusolMint: Pk;
  stakePool: Pk; // the fork StakePool (canonical-primary lst_stake_pool leg)
}
/** MarketOracle (anchor). Offsets from market_oracle.rs, +8 disc: collateral_mint @8, pyth_feed_id @40,
 * orca_pool @104, raydium_pool @136, lst_stake_pool @272, canonical_primary u8 @305,
 * liquidity_haircut_bps u16 @306. */
export function checkMarketOracle(
  acct: Acct | null,
  exp: MarketOracleExpect,
): string | null {
  if (!acct) return "market oracle account is missing/unreadable";
  if (!acct.owner.equals(exp.fusdCoreProgram))
    return `owner ${b58(acct.owner)} is not the fusd-core program`;
  if (acct.data.length !== MARKET_ORACLE_SPACE)
    return `size ${acct.data.length} != ${MARKET_ORACLE_SPACE}`;
  if (!acct.data.subarray(0, 8).equals(anchorDiscriminator("MarketOracle")))
    return "wrong anchor discriminator (not a MarketOracle account)";
  const d = acct.data;
  if (!new PublicKey(d.subarray(8, 40)).equals(exp.fusolMint))
    return "collateral_mint is not the fuSOL mint";
  if (d[305] !== 1)
    return `canonical_primary ${d[305]} != 1 (fuSOL must price as sol_usd × pool_rate)`;
  const haircut = d.readUInt16LE(306);
  if (haircut < 1 || haircut > MAX_LIQUIDITY_HAIRCUT_BPS)
    return `liquidity_haircut_bps ${haircut} out of [1, ${MAX_LIQUIDITY_HAIRCUT_BPS}]`;
  if (!new PublicKey(d.subarray(272, 304)).equals(exp.stakePool))
    return `lst_stake_pool is ${b58(new PublicKey(d.subarray(272, 304)))}, expected the fork StakePool ${b58(exp.stakePool)}`;
  if (!d.subarray(40, 72).equals(PYTH_SOL_USD_FEED_ID))
    return "pyth_feed_id != PYTH_SOL_USD_FEED_ID";
  if (!new PublicKey(d.subarray(104, 136)).equals(ZERO_KEY))
    return "orca_pool is set (canonical-primary mode requires no DEX pools)";
  if (!new PublicKey(d.subarray(136, 168)).equals(ZERO_KEY))
    return "raydium_pool is set (canonical-primary mode requires no DEX pools)";
  return null;
}

export interface MarketExpect {
  fusdCoreProgram: Pk;
  fusolMint: Pk;
}
/** Market (anchor). Offsets from market.rs, +8 disc: collateral_mint @8, debt_ceiling u64 @170,
 * liq_infra_flags u8 @500. */
export function checkMarket(
  acct: Acct | null,
  exp: MarketExpect,
): string | null {
  if (!acct) return "market account is missing/unreadable";
  if (!acct.owner.equals(exp.fusdCoreProgram))
    return `owner ${b58(acct.owner)} is not the fusd-core program`;
  if (acct.data.length !== MARKET_SPACE)
    return `size ${acct.data.length} != ${MARKET_SPACE}`;
  if (!acct.data.subarray(0, 8).equals(anchorDiscriminator("Market")))
    return "wrong anchor discriminator (not a Market account)";
  const d = acct.data;
  if (!new PublicKey(d.subarray(8, 40)).equals(exp.fusolMint))
    return "collateral_mint is not the fuSOL mint";
  const debtCeiling = d.readBigUInt64LE(170);
  if (debtCeiling <= 0n)
    return "debt_ceiling is 0 (the market cannot back any debt)";
  const flags = d[500];
  if ((flags & LIQ_INFRA_READY_MASK) !== LIQ_INFRA_READY_MASK)
    return `liq_infra_flags ${flags} not ready (reactor pool + insurance buffer must be initialized: ${LIQ_INFRA_READY_MASK} bits set)`;
  return null;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// CONFIG
// ═══════════════════════════════════════════════════════════════════════════════════════════════

export interface ProgramExpectRaw {
  expectUpgradeAuthority: string; // "none" | base58
  expectedElfSha256?: string; // 32-byte hex (optional)
}
export interface VerifyConfig {
  rpcUrl: string;
  phase?: "pre-seal" | "sealed";
  fusolMint: string;
  maintenanceVault: string;
  stakePool: string;
  validatorList: string;
  reserveStake: string;
  controllerConfig: string;
  poolAuthority: string;
  depositAuthority: string;
  maintenanceAuthority: string;
  poolWithdrawAuthority: string;
  fusdCoreProgram: string;
  fusolMarket: string;
  programs: {
    controller: ProgramExpectRaw;
    fork: ProgramExpectRaw;
    fusdCore?: ProgramExpectRaw;
  };
}

const ADDRESS_FIELDS = [
  "fusolMint",
  "maintenanceVault",
  "stakePool",
  "validatorList",
  "reserveStake",
  "controllerConfig",
  "poolAuthority",
  "depositAuthority",
  "maintenanceAuthority",
  "poolWithdrawAuthority",
  "fusdCoreProgram",
  "fusolMarket",
] as const;

/** Fail-fast validation of a config (base58 fields, valid phase enum, "none"|base58 authorities,
 * hex hashes). Throws on anything malformed. */
export function validateConfig(cfg: any): asserts cfg is VerifyConfig {
  const bail = (m: string): never => {
    throw new Error(`config: ${m}`);
  };
  if (!cfg || typeof cfg !== "object") bail("must be a JSON object");
  if (typeof cfg.rpcUrl !== "string" || !cfg.rpcUrl)
    bail("rpcUrl must be a non-empty string");
  if (
    cfg.phase !== undefined &&
    cfg.phase !== "pre-seal" &&
    cfg.phase !== "sealed"
  )
    bail(`phase must be "pre-seal" or "sealed" (got ${cfg.phase})`);
  for (const f of ADDRESS_FIELDS) {
    if (typeof cfg[f] !== "string") bail(`${f} must be a base58 string`);
    try {
      new PublicKey(cfg[f]);
    } catch {
      bail(`${f} is not a valid base58 pubkey: ${cfg[f]}`);
    }
  }
  if (!cfg.programs || typeof cfg.programs !== "object")
    bail("programs block is required");
  const checkProg = (name: string, required: boolean): void => {
    const p = cfg.programs[name];
    if (p === undefined) {
      if (required) bail(`programs.${name} is required`);
      return;
    }
    if (typeof p !== "object") bail(`programs.${name} must be an object`);
    const ua = p.expectUpgradeAuthority;
    if (ua !== "none") {
      if (typeof ua !== "string")
        bail(
          `programs.${name}.expectUpgradeAuthority must be "none" or a base58 pubkey`,
        );
      try {
        new PublicKey(ua);
      } catch {
        bail(
          `programs.${name}.expectUpgradeAuthority is not "none" nor a valid pubkey: ${ua}`,
        );
      }
    }
    if (
      p.expectedElfSha256 !== undefined &&
      (typeof p.expectedElfSha256 !== "string" ||
        !/^(0x)?[0-9a-fA-F]{64}$/.test(p.expectedElfSha256))
    )
      bail(`programs.${name}.expectedElfSha256 must be a 32-byte hex string`);
  };
  checkProg("controller", true);
  checkProg("fork", true);
  checkProg("fusdCore", false);
}

const marketPda = (fusdCore: Pk, mint: Pk): Pk =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("market"), mint.toBuffer()],
    fusdCore,
  )[0];
const oraclePda = (fusdCore: Pk, mint: Pk): Pk =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("oracle"), mint.toBuffer()],
    fusdCore,
  )[0];

/** Re-derive every PDA and assert the config's value matches (a config that lies about a PDA fails).
 * Returns the list of mismatches (empty == all derivations agree). */
export function derivedPdaErrors(cfg: VerifyConfig): string[] {
  const errs: string[] = [];
  const fusolMint = new PublicKey(cfg.fusolMint);
  const stakePool = new PublicKey(cfg.stakePool);
  const fusdCore = new PublicKey(cfg.fusdCoreProgram);
  const cmp = (label: string, configured: string, derived: Pk): void => {
    if (!new PublicKey(configured).equals(derived))
      errs.push(`${label}: config ${configured} != derived ${b58(derived)}`);
  };
  cmp("controllerConfig", cfg.controllerConfig, deriveControllerConfig());
  cmp("poolAuthority", cfg.poolAuthority, derivePoolAuthority());
  cmp("depositAuthority", cfg.depositAuthority, deriveDepositAuthority());
  cmp(
    "maintenanceAuthority",
    cfg.maintenanceAuthority,
    deriveMaintenanceAuthority(),
  );
  cmp(
    "poolWithdrawAuthority",
    cfg.poolWithdrawAuthority,
    derivePoolWithdrawAuthority(stakePool),
  );
  cmp("fusolMarket", cfg.fusolMarket, marketPda(fusdCore, fusolMint));
  return errs;
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// ORCHESTRATION (live RPC)
// ═══════════════════════════════════════════════════════════════════════════════════════════════

// PASS gates open; FAIL and NOT_VERIFIED (a missing pinned hash — a launch gate cannot certify
// without it) both fail the gate; INFO is a purely informational line (e.g. an intentionally
// out-of-scope check) that NEVER gates and must never carry a real assertion.
export type CheckStatus = "PASS" | "FAIL" | "NOT_VERIFIED" | "INFO";
export interface CheckResult {
  id: string;
  title: string;
  status: CheckStatus;
  detail?: string;
  notes?: string[];
}

interface Args {
  configPath?: string;
  json: boolean;
  phase?: "pre-seal" | "sealed";
  commitment?: Commitment;
}
function parseArgs(argv: string[]): Args {
  const a: Args = { json: false };
  for (let i = 0; i < argv.length; i++) {
    const t = argv[i];
    if (t === "--json") a.json = true;
    else if (t === "--config") a.configPath = argv[++i];
    else if (t.startsWith("--config="))
      a.configPath = t.slice("--config=".length);
    else if (t === "--phase") a.phase = argv[++i] as any;
    else if (t.startsWith("--phase="))
      a.phase = t.slice("--phase=".length) as any;
    else if (t === "--commitment") a.commitment = argv[++i] as Commitment;
    else if (t.startsWith("--commitment="))
      a.commitment = t.slice("--commitment=".length) as Commitment;
    else if (!t.startsWith("--") && !a.configPath) a.configPath = t;
  }
  return a;
}

const programDataAddress = (programId: Pk): Pk =>
  PublicKey.findProgramAddressSync(
    [programId.toBuffer()],
    BPF_LOADER_UPGRADEABLE,
  )[0];

const toAuthority = (ua: string): "none" | Pk =>
  ua === "none" ? "none" : new PublicKey(ua);

/** One RPC fetch per account, fail-closed: an RPC error becomes a per-account error string (never a
 * throw that skips the rest). Returns { acct, err } where err is set on RPC failure. */
async function fetchAll(
  conn: Connection,
  keys: Map<string, Pk>,
  commitment: Commitment,
): Promise<Map<string, { acct: Acct | null; err: string | null }>> {
  const names = [...keys.keys()];
  const settled = await Promise.allSettled(
    names.map((n) => conn.getAccountInfo(keys.get(n)!, commitment)),
  );
  const out = new Map<string, { acct: Acct | null; err: string | null }>();
  names.forEach((n, i) => {
    const r = settled[i];
    if (r.status === "rejected")
      out.set(n, {
        acct: null,
        err: `RPC error: ${r.reason?.message ?? r.reason}`,
      });
    else if (r.value === null)
      out.set(n, { acct: null, err: null }); // missing (the pure check reports it)
    else
      out.set(n, {
        acct: {
          owner: r.value.owner,
          data: r.value.data,
          executable: r.value.executable,
        },
        err: null,
      });
  });
  return out;
}

async function run(
  cfg: VerifyConfig,
  commitment: Commitment = "finalized",
): Promise<{ results: CheckResult[]; pass: boolean }> {
  const results: CheckResult[] = [];
  const push = (
    id: string,
    title: string,
    status: CheckStatus,
    detail?: string,
    notes?: string[],
  ): void => {
    results.push({ id, title, status, detail, notes });
  };
  // A pure-check result (string|null error) -> a CheckResult.
  const record = (
    id: string,
    title: string,
    err: string | null,
    notes?: string[],
  ): void =>
    push(id, title, err === null ? "PASS" : "FAIL", err ?? undefined, notes);

  const fusolMint = new PublicKey(cfg.fusolMint);
  const stakePool = new PublicKey(cfg.stakePool);
  const fusdCore = new PublicKey(cfg.fusdCoreProgram);
  const poolWithdraw = derivePoolWithdrawAuthority(stakePool);

  // CHECK 0 — derived PDAs match config (a config that lies about a PDA must fail before any read).
  const pdaErrs = derivedPdaErrors(cfg);
  push(
    "0",
    "Derived PDAs match config",
    pdaErrs.length === 0 ? "PASS" : "FAIL",
    pdaErrs.join("; ") || undefined,
  );

  // Assemble the account set to fetch.
  const controllerPd = programDataAddress(CONTROLLER_PROGRAM_ID);
  const forkPd = programDataAddress(STAKE_POOL_FORK_ID);
  const fusdCorePd = programDataAddress(fusdCore);
  const oracle = oraclePda(fusdCore, fusolMint);

  const keys = new Map<string, Pk>([
    ["controllerProgram", CONTROLLER_PROGRAM_ID],
    ["controllerProgramData", controllerPd],
    ["forkProgram", STAKE_POOL_FORK_ID],
    ["forkProgramData", forkPd],
    ["fusdCoreProgram", fusdCore],
    ["fusdCoreProgramData", fusdCorePd],
    ["fusolMint", fusolMint],
    ["maintenanceVault", new PublicKey(cfg.maintenanceVault)],
    ["stakePool", stakePool],
    ["validatorList", new PublicKey(cfg.validatorList)],
    ["reserveStake", new PublicKey(cfg.reserveStake)],
    ["controllerConfig", new PublicKey(cfg.controllerConfig)],
    ["market", new PublicKey(cfg.fusolMarket)],
    ["oracle", oracle],
  ]);
  // Read at `finalized` by default: this gate certifies an IRREVERSIBLE action (upgrade
  // authorities actually None). A `confirmed` read of a not-yet-finalized renounce/seal that a
  // rare fork rollback later reverts would flash a false PASS; `finalized` closes that. Override
  // with --commitment only for a dev fork where finality lags.
  const fetched = await fetchAll(new Connection(cfg.rpcUrl, commitment), keys, commitment);
  const get = (name: string): { acct: Acct | null; err: string | null } =>
    fetched.get(name)!;

  // Program descriptors for checks 1 + 2 (fusd-core's program block is optional).
  const programs: {
    id: string;
    label: string;
    progName: string;
    pdName: string;
    pd: Pk;
    expect?: ProgramExpectRaw;
  }[] = [
    {
      id: "controller",
      label: "controller",
      progName: "controllerProgram",
      pdName: "controllerProgramData",
      pd: controllerPd,
      expect: cfg.programs.controller,
    },
    {
      id: "fork",
      label: "stake-pool fork",
      progName: "forkProgram",
      pdName: "forkProgramData",
      pd: forkPd,
      expect: cfg.programs.fork,
    },
    {
      id: "fusdCore",
      label: "fusd-core",
      progName: "fusdCoreProgram",
      pdName: "fusdCoreProgramData",
      pd: fusdCorePd,
      expect: cfg.programs.fusdCore,
    },
  ];

  // fusd-core is intentionally upgradeable through the guarded Phases 1-3 (spec §11), so its
  // program block is optional — but an omission must be VISIBLE (not a silent skip) so an
  // operator who meant to check it sees that it was not.
  if (!cfg.programs.fusdCore)
    push(
      "1.fusdCore",
      "Program immutable — fusd-core",
      "INFO",
      "programs.fusdCore absent — fusd-core is deliberately upgradeable through Phases 1-3; add the block to assert its authority/hash",
    );

  // CHECK 1 — programs executable + upgrade authority.
  for (const pr of programs) {
    if (!pr.expect) continue; // fusd-core program block is optional (reported NOT_CHECKED above)
    const title = `Program immutable — ${pr.label} (expect ${pr.expect.expectUpgradeAuthority})`;
    const p = get(pr.progName);
    const pd = get(pr.pdName);
    const rpcErr = p.err ?? pd.err;
    if (rpcErr) push(`1.${pr.id}`, title, "FAIL", rpcErr);
    else
      record(
        `1.${pr.id}`,
        title,
        checkProgramImmutability(
          p.acct,
          pd.acct,
          pr.pd,
          toAuthority(pr.expect.expectUpgradeAuthority),
        ),
      );
  }

  // CHECK 2 — program ELF hash (+ security.txt notes).
  for (const pr of programs) {
    if (!pr.expect) continue;
    const title = `Program ELF hash — ${pr.label}`;
    const pd = get(pr.pdName);
    if (pd.err) {
      push(`2.${pr.id}`, title, "FAIL", pd.err);
      continue;
    }
    const r = checkProgramElfHash(pd.acct, pr.expect.expectedElfSha256);
    const notes: string[] = [];
    if (pd.acct) {
      const st = extractSecurityTxt(pd.acct.data);
      notes.push(
        st
          ? `security.txt: ${st.name ?? "(present)"}${st.contacts ? ` — contacts ${st.contacts}` : ""}`
          : "security.txt: not present",
      );
    }
    push(`2.${pr.id}`, title, r.status, r.detail, notes);
  }

  // CHECK 3 — fuSOL mint.
  const mint = get("fusolMint");
  const poolAcct = get("stakePool");
  if (mint.err) {
    push("3", "fuSOL mint", "FAIL", mint.err);
  } else {
    let err = checkFusolMint(mint.acct, poolWithdraw);
    // Supply == pool_token_supply cross-invariant (needs the parsed pool).
    if (err === null && mint.acct) {
      if (poolAcct.err) err = `cannot cross-check supply: ${poolAcct.err}`;
      else if (!poolAcct.acct)
        err = "cannot cross-check supply: stake pool account missing";
      else {
        try {
          err = checkMintSupplyMatchesPool(
            mint.acct.data,
            parseStakePool(poolAcct.acct.data).poolTokenSupply,
          );
        } catch (e: any) {
          err = `cannot cross-check supply: stake pool parse failed: ${e.message}`;
        }
      }
    }
    record(
      "3",
      "fuSOL mint (9 decimals, mint authority = pool withdraw PDA, freeze None, supply)",
      err,
    );
  }

  // CHECK 4 — stake pool.
  if (poolAcct.err)
    push("4", "StakePool authority graph + fee schedule", "FAIL", poolAcct.err);
  else
    record(
      "4",
      "StakePool authority graph + fee schedule",
      checkStakePool(poolAcct.acct, {
        poolAuthority: derivePoolAuthority(),
        depositAuthority: deriveDepositAuthority(),
        validatorList: new PublicKey(cfg.validatorList),
        reserveStake: new PublicKey(cfg.reserveStake),
        fusolMint,
        maintenanceVault: new PublicKey(cfg.maintenanceVault),
      }),
    );

  // CHECK 5 — validator list.
  const vlist = get("validatorList");
  if (vlist.err)
    push("5", "ValidatorList (max_validators = 1024)", "FAIL", vlist.err);
  else
    record(
      "5",
      "ValidatorList (max_validators = 1024)",
      checkValidatorList(vlist.acct),
    );

  // CHECK 6 — maintenance vault.
  const vault = get("maintenanceVault");
  if (vault.err)
    push(
      "6",
      "Maintenance vault (fuSOL token account, maintenance PDA authority)",
      "FAIL",
      vault.err,
    );
  else
    record(
      "6",
      "Maintenance vault (fuSOL token account, maintenance PDA authority)",
      checkMaintenanceVault(
        vault.acct,
        fusolMint,
        deriveMaintenanceAuthority(),
      ),
    );

  // CHECK 7 — reserve stake.
  const reserve = get("reserveStake");
  if (reserve.err)
    push(
      "7",
      "Reserve stake (staker = withdrawer = pool withdraw PDA, no lockup)",
      "FAIL",
      reserve.err,
    );
  else
    record(
      "7",
      "Reserve stake (staker = withdrawer = pool withdraw PDA, no lockup)",
      checkReserveStake(reserve.acct, poolWithdraw),
    );

  // CHECK 8 — controller config.
  const cc = get("controllerConfig");
  if (cc.err)
    push(
      "8",
      "ControllerConfig (sealed, recorded address set, canonical bumps)",
      "FAIL",
      cc.err,
    );
  else
    record(
      "8",
      "ControllerConfig (sealed, recorded address set, canonical bumps)",
      checkControllerConfig(cc.acct, {
        stakePoolProgram: STAKE_POOL_FORK_ID,
        stakePool,
        validatorList: new PublicKey(cfg.validatorList),
        reserveStake: new PublicKey(cfg.reserveStake),
        fusolMint,
        poolWithdrawAuthority: poolWithdraw,
        maintenanceVault: new PublicKey(cfg.maintenanceVault),
        fusdCoreProgram: fusdCore,
        bump: canonicalBump(deriveControllerConfigSeeds()),
        poolAuthorityBump: canonicalBump([Buffer.from("pool_authority")]),
        depositAuthorityBump: canonicalBump([Buffer.from("deposit_authority")]),
        maintenanceAuthorityBump: canonicalBump([Buffer.from("maintenance")]),
      }),
    );

  // CHECK 9 — fusd-core fuSOL market + oracle.
  const oracleAcct = get("oracle");
  if (oracleAcct.err)
    push(
      "9.oracle",
      "fuSOL MarketOracle (canonical-primary, lst_stake_pool, no DEX)",
      "FAIL",
      oracleAcct.err,
    );
  else
    record(
      "9.oracle",
      "fuSOL MarketOracle (canonical-primary, lst_stake_pool, no DEX)",
      checkMarketOracle(oracleAcct.acct, {
        fusdCoreProgram: fusdCore,
        fusolMint,
        stakePool,
      }),
    );
  const marketAcct = get("market");
  if (marketAcct.err)
    push(
      "9.market",
      "fuSOL Market (debt_ceiling > 0, liq infra ready, collateral = fuSOL)",
      "FAIL",
      marketAcct.err,
    );
  else
    record(
      "9.market",
      "fuSOL Market (debt_ceiling > 0, liq infra ready, collateral = fuSOL)",
      checkMarket(marketAcct.acct, { fusdCoreProgram: fusdCore, fusolMint }),
    );

  // CHECK 10 — emergency posture summary (the observable no-freeze fact + source-verified controls).
  const freezeErr = mint.err ? mint.err : checkMintFreezeNone(mint.acct);
  push(
    "10",
    "Emergency posture (no freeze / no clawback)",
    freezeErr === null ? "PASS" : "FAIL",
    freezeErr ?? undefined,
    [
      "fuSOL mint freeze authority = None (on-chain-observable; see check 3).",
      "SOL withdrawals never authority-gated: sol_withdraw_authority = None (on-chain-observable; see check 4).",
      "Controller CPI-allowlist: SetFee/SetManager/SetStaker/SetFundingAuthority/metadata builders absent from the controller binary (SOURCE-VERIFIED, on-chain-unobservable — see the audit's positive controls).",
      "No fee setter: the pool fee schedule is fixed at initialize_pool and pinned in check 4 (SOURCE-VERIFIED positive control).",
      "No clawback: a legacy SPL mint has no clawback and withdrawals bypass the controller (SOURCE-VERIFIED).",
    ],
  );

  // INFO lines never gate; every other status (FAIL / NOT_VERIFIED) does. So a genuine check that
  // could not run still fails closed, while an intentionally-out-of-scope note does not.
  const pass = results.every((r) => r.status === "PASS" || r.status === "INFO");
  return { results, pass };
}

// Helper: mint freeze-None sub-check reused by the emergency-posture summary (check 10).
export function checkMintFreezeNone(acct: Acct | null): string | null {
  if (!acct) return "mint account is missing/unreadable";
  if (acct.data.length < 50)
    return `mint too short (${acct.data.length}) to read the freeze authority`;
  return acct.data.readUInt32LE(46) === 0
    ? null
    : "freeze authority is set (must be None)";
}

// Canonical bump helpers for the ControllerConfig check.
const deriveControllerConfigSeeds = (): Buffer[] => [Buffer.from("controller")];
function canonicalBump(seeds: Buffer[]): number {
  return PublicKey.findProgramAddressSync(seeds, CONTROLLER_PROGRAM_ID)[1];
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// REPORT
// ═══════════════════════════════════════════════════════════════════════════════════════════════

const MARK: Record<CheckStatus, string> = {
  PASS: "✓",
  FAIL: "✗",
  NOT_VERIFIED: "⚠",
  INFO: "·",
};

function printReport(
  results: CheckResult[],
  pass: boolean,
  phase: string,
): void {
  console.log(`\nfuSOL deployment verifier — phase: ${phase}`);
  console.log("=".repeat(78));
  for (const r of results) {
    console.log(`${MARK[r.status]} [${r.id}] ${r.title}`);
    if (r.status !== "PASS" && r.detail)
      console.log(`      ${r.status}: ${r.detail}`);
    else if (r.status === "PASS" && r.detail) console.log(`      ${r.detail}`);
    for (const n of r.notes ?? []) console.log(`      · ${n}`);
  }
  console.log("=".repeat(78));
  const fails = results.filter(
    (r) => r.status !== "PASS" && r.status !== "INFO",
  );
  console.log(
    pass
      ? "RESULT: PASS — all checks passed."
      : `RESULT: FAIL — ${fails.length} check(s) did not pass: ${fails.map((f) => f.id).join(", ")}`,
  );
}

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2));
  const path = args.configPath ?? process.env.FUSOL_VERIFY_CONFIG;
  if (!path)
    throw new Error(
      "no config: pass a path (positional or --config), or set FUSOL_VERIFY_CONFIG",
    );
  const cfg = JSON.parse(fs.readFileSync(path, "utf8"));
  validateConfig(cfg);

  // Phase is framing only — the config's per-program expectUpgradeAuthority is authoritative.
  const allNone = Object.values(cfg.programs).every(
    (p: any) => !p || p.expectUpgradeAuthority === "none",
  );
  const phase = args.phase ?? cfg.phase ?? (allNone ? "sealed" : "pre-seal");
  // A sealed run reads at `finalized` (certifying an irreversible renounce); a pre-seal run may
  // use `confirmed` for speed since it asserts a still-mutable state. --commitment overrides.
  const commitment: Commitment =
    args.commitment ?? (phase === "sealed" ? "finalized" : "confirmed");

  const { results, pass } = await run(cfg, commitment);

  if (args.json) {
    console.log(JSON.stringify({ phase, pass, checks: results }, null, 2));
  } else {
    printReport(results, pass, phase);
  }
  if (!pass) process.exit(1);
}

if (require.main === module) {
  main().catch((e) => {
    console.error(e);
    process.exit(1);
  });
}
