/**
 * Shared helpers for the permissionless keeper bots (liquidator.ts, redeemer.ts) — provider/program
 * setup, the per-market PDA bundle, position scanning, the liquidation health predicate (a faithful
 * BigInt port of the on-chain cdp::is_healthy, priced at debt_spot), and on-demand ATA creation.
 *
 * These bots are PERMISSIONLESS reference implementations: anyone can run them, kept profitable by the
 * liquidation bonus + the redemption-fee spread. The on-chain instruction is the source of truth — the
 * bots only select candidates and submit; a healthy position is rejected on-chain (no harm, just a
 * wasted tx), so the off-chain math is an approximation tuned to avoid obviously-wasted submissions.
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import * as os from "os";

export const { PublicKey, Keypair, Connection } = anchor.web3;
export type Pk = anchor.web3.PublicKey;
export const BN = anchor.BN;

export const TOKEN_PROGRAM = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROGRAM = new PublicKey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

// On-chain constants (programs/fusd-core/src/constants.rs).
export const RAY = 10n ** 27n;
export const BPS = 10_000n;
export const FUSD_DECIMALS = 6;
export const SECONDS_PER_YEAR = 31_536_000n;
export const MAX_PRICE_STALENESS_SLOTS = 250n;
export const ZOMBIE_BUCKET = 256;
export const MAX_REDEMPTION_CANDIDATES = 20;

export const seed = (s: string) => Buffer.from(s);
export function pda(seeds: (Buffer | Pk)[], pid: Pk): Pk {
  return PublicKey.findProgramAddressSync(seeds.map((s) => (s instanceof PublicKey ? s.toBuffer() : s)), pid)[0];
}
export const ataFor = (mint: Pk, owner: Pk): Pk => pda([owner, TOKEN_PROGRAM, mint], ATA_PROGRAM);
export const log = (m: string) => console.log(`${new Date().toISOString()} ${m}`);

export function loadWallet(): anchor.Wallet {
  const path = process.env.ANCHOR_WALLET || `${os.homedir()}/.config/solana/id.json`;
  return new anchor.Wallet(Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(path, "utf8")))));
}

/** Anchor program + provider (IDL from target/idl/fusd_core.json). Pass a wallet to override the
 * default keypair file — e.g. a throwaway `new anchor.Wallet(Keypair.generate())` for read-only use. */
export function makeProgram(wallet: anchor.Wallet = loadWallet()) {
  const url = process.env.ANCHOR_PROVIDER_URL || "http://127.0.0.1:8899";
  const provider = new anchor.AnchorProvider(new Connection(url, "confirmed"), wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);
  const idl = JSON.parse(fs.readFileSync(`${__dirname}/../target/idl/fusd_core.json`, "utf8"));
  const program: any = new anchor.Program(idl as anchor.Idl, provider);
  return { program, provider, pid: program.programId as Pk, me: wallet.publicKey as Pk, url };
}

/** Every PDA the liquidate/redeem instructions need for one market (seeds from constants.rs). */
export function bundle(pid: Pk, coll: Pk) {
  return {
    coll,
    config: pda([seed("config")], pid),
    fusdMint: pda([seed("fusd_mint")], pid),
    mintAuthority: pda([seed("mint_authority")], pid),
    market: pda([seed("market"), coll], pid),
    collateralVault: pda([seed("coll_vault"), coll], pid),
    redemptionBitmap: pda([seed("redeem_bitmap"), coll], pid),
    reactorPool: pda([seed("reactor"), coll], pid),
    ess: pda([seed("ess"), coll], pid),
    reactorFusdVault: pda([seed("reactor_fusd"), coll], pid),
    reactorCollVault: pda([seed("reactor_coll"), coll], pid),
    buffer: pda([seed("buffer"), coll], pid),
    bufferFusdVault: pda([seed("buffer_fusd"), coll], pid),
  };
}
export const positionPda = (pid: Pk, coll: Pk, owner: Pk): Pk => pda([seed("position"), coll, owner], pid);

/** BN/number/string → bigint (Anchor account fields decode as BN). */
export const bi = (v: any): bigint => BigInt(v.toString());

/** A scanned position (BigInt-normalized). collateral_mint is at byte offset 40 (8 disc + 32 owner). */
export interface ScannedPosition {
  pubkey: Pk;
  owner: Pk;
  ink: bigint;
  recordedDebt: bigint;
  userRateBps: number;
  lastDebtUpdate: bigint;
  bucket: number;
}
// Default: getProgramAccounts with a memcmp on collateral_mint (offset 40) — the permissionless
// discovery path, but it needs a gPA-capable RPC. If `watch` is given, fetch exactly those position
// accounts directly via getAccountInfo and keep the ones for `coll`: the path for RPCs that throttle
// or disable gPA (most providers) or don't implement it (surfpool), fed by an indexer/monitor list.
export async function scanPositions(program: any, coll: Pk, watch?: Pk[]): Promise<ScannedPosition[]> {
  let accts: any[];
  if (watch && watch.length) {
    const fetched = await Promise.all(watch.map((pk) => program.account.position.fetchNullable(pk)));
    accts = fetched
      .map((account, i) => (account ? { publicKey: watch[i], account } : null))
      .filter((a) => a && a.account.collateralMint.equals(coll));
  } else {
    accts = await program.account.position.all([{ memcmp: { offset: 40, bytes: coll.toBase58() } }]);
  }
  return accts.map((a: any) => ({
    pubkey: a.publicKey,
    owner: a.account.owner,
    ink: bi(a.account.ink),
    recordedDebt: bi(a.account.recordedDebt),
    userRateBps: Number(a.account.userRateBps),
    lastDebtUpdate: bi(a.account.lastDebtUpdate),
    bucket: Number(a.account.bucket),
  }));
}

/** Present (interest-accrued) debt — port of accrual::accrued_interest + the SDK currentDebt. */
export function currentDebt(recordedDebt: bigint, rateBps: number, lastDebtUpdate: bigint, nowSecs: bigint): bigint {
  const period = nowSecs > lastDebtUpdate ? nowSecs - lastDebtUpdate : 0n;
  const accrued = (recordedDebt * BigInt(rateBps) * period) / (SECONDS_PER_YEAR * BPS);
  return recordedDebt + accrued;
}
/** cdp::is_healthy at the HIGH debt_spot price: liquidatable when present debt exceeds the max. */
export function isLiquidatable(ink: bigint, presentDebt: bigint, debtSpot: bigint, mcrBps: number): boolean {
  if (presentDebt === 0n || mcrBps === 0 || debtSpot === 0n) return false;
  const collValue = (ink * debtSpot) / RAY; // fUSD-native
  const maxDebt = (collValue * BPS) / BigInt(mcrBps);
  return presentDebt > maxDebt;
}

/** Ensure `owner`'s ATA for `mint` exists, creating it (payer = provider wallet) if missing. */
export async function ensureAta(provider: anchor.AnchorProvider, mint: Pk, owner: Pk): Promise<Pk> {
  const ata = ataFor(mint, owner);
  if (!(await provider.connection.getAccountInfo(ata))) {
    let spl: any;
    try { spl = require("@solana/spl-token"); }
    catch { throw new Error("ATA creation needs @solana/spl-token (npm i @solana/spl-token)"); }
    const tx = new anchor.web3.Transaction().add(
      spl.createAssociatedTokenAccountInstruction(provider.wallet.publicKey, ata, owner, mint),
    );
    await provider.sendAndConfirm(tx);
  }
  return ata;
}

/** First word of a SendTransactionError/AnchorError message, for compact one-line logging. */
export const errLine = (e: any): string => (e?.message || String(e)).split("\n")[0];
