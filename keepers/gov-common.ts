/**
 * Shared helpers for the governance client scripts (set-param.ts, keys-sync.ts) — a tiny flag parser,
 * the governance PDAs, the param enum/clamp tables, and the safety-first "build then dry-run unless
 * --send" submitter. These scripts MUTATE protocol params + authority, so the default is to PRINT the
 * instruction as governance-proposal-ready JSON (e.g. for a multisig) and only sign+submit on an
 * explicit --send.
 */
import * as fs from "fs";
import * as anchor from "@coral-xyz/anchor";
import { PublicKey, Pk, log } from "./common";
import {
  deriveConfig, deriveGovGate, deriveBackstop, deriveTimelock, deriveGlobalTimelock, deriveMarket,
  deriveMarketOracle,
} from "../sdk/src";

export { PublicKey, log };
export type { Pk };

export interface Flags { _: string[]; get: (k: string) => string | undefined; has: (k: string) => boolean; }
/** Parse `--k v` (value) and bare `--flag` (boolean) plus positionals. */
export function flags(argv: string[]): Flags {
  const map: Record<string, string> = {}; const pos: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) {
      const k = a.slice(2); const next = argv[i + 1];
      if (next === undefined || next.startsWith("--")) map[k] = "true";
      else { map[k] = next; i++; }
    } else pos.push(a);
  }
  return { _: pos, get: (k) => map[k], has: (k) => k in map };
}

// PDA derivation lives in the SDK (sdk/src/index.ts, seeds pinned by sdk/test/health.spec.ts) —
// these are thin arg-order adapters so the gov scripts read `xPda(pid, ...)` uniformly.
export const govPdas = (pid: Pk) => ({
  config: deriveConfig(pid),
  govGate: deriveGovGate(pid),
  backstop: deriveBackstop(pid),
});
export const timelockPda = (pid: Pk, nonce: bigint): Pk => deriveTimelock(nonce, pid);
export const gtimelockPda = (pid: Pk, nonce: bigint): Pk => deriveGlobalTimelock(nonce, pid);
export const marketPda = (pid: Pk, coll: Pk): Pk => deriveMarket(coll, pid);
export const marketOraclePda = (pid: Pk, coll: Pk): Pk => deriveMarketOracle(coll, pid);

/** Print the script's doc-comment header as CLI usage. */
export const printUsage = (file: string) => console.log(fs.readFileSync(file, "utf8").split("*/")[0].replace("/**", ""));

/** Shared CLI entry wrapper — one error format + exit code across the gov scripts. */
export const runCli = (main: () => Promise<unknown>): void => {
  main().catch((e) => { console.error("ERROR:", e.message || e); process.exit(1); });
};

/** The signer for a mutating instruction: the loaded wallet, or a --authority override (dry-run
 * proposals only — in --send the loaded wallet must actually sign, so an override must match it). */
export function authorityOf(f: Flags, me: Pk, send: boolean): Pk {
  const o = f.get("authority"); if (!o) return me;
  const pk = new PublicKey(o);
  if (send && !pk.equals(me)) throw new Error("--authority override is for dry-run proposals only; in --send the loaded wallet must be the signer");
  return pk;
}

/** PascalCase IDL variant → the camelCase key Anchor expects as the enum arg, e.g. "LiqBonus" → {liqBonus:{}}. */
export const camel = (s: string): string => s.charAt(0).toLowerCase() + s.slice(1);

/** Match a user-supplied param name (case-insensitive) against the IDL's variant list. */
export function resolveVariant(idlVariants: string[], input: string): { name: string; arg: any } {
  const hit = idlVariants.find((v) => camel(v).toLowerCase() === input.toLowerCase() || v.toLowerCase() === input.toLowerCase());
  if (!hit) throw new Error(`unknown param "${input}". valid: ${idlVariants.map(camel).join(", ")}`);
  return { name: hit, arg: { [camel(hit)]: {} } };
}

export interface Clamp { unit: string; min?: number; max?: number; note?: string; }
// `min` only where a NON-ZERO floor exists — reqValue already rejects negatives, so `min: 0` never warns.
export const MARKET_CLAMPS: Record<string, Clamp> = {
  Mcr: { unit: "bps", min: 10000, max: 30000 },
  DebtCeiling: { unit: "fUSD-native", note: "0 pauses new debt; no upper clamp" },
  RedemptionFee: { unit: "bps", max: 500 },
  LiqGasComp: { unit: "bps", max: 1000 },
  RateLimitCap: { unit: "fUSD-native/window", note: "0 disables; no upper clamp" },
  Ccr: { unit: "bps", note: "0 disables; otherwise [10000, 30000]" },
  LiqBonus: { unit: "bps", max: 2000, note: "0 = collar off (seize all)" },
  MinDebt: { unit: "fUSD-native", max: 10_000_000_000, note: "0 disables" },
  RateAdjustCooldown: { unit: "secs", max: 2_592_000, note: "0 disables" },
  KeeperReward: { unit: "bps", max: 1000, note: "0 disables" },
  BorrowFee: { unit: "bps", max: 500, note: "0 disables (C7 upfront borrow fee)" },
  BadDebtPaydown: { unit: "bps", max: 10_000, note: "0 disables (C16 auto bad-debt paydown)" },
  RedemptionBaseRateMax: { unit: "bps", max: 500, note: "0 disables the dynamic base rate (C9)" },
  OracleMaxConf: { unit: "bps", min: 1, max: 500 },
  OracleMaxDeviation: { unit: "bps", min: 1, max: 500 },
  OracleTwapDivergence: { unit: "bps", min: 1, max: 1000, note: "must stay <= oracleLiqDivergence" },
  OracleLiqDivergence: { unit: "bps", max: 10_000, note: "0 disables; must stay >= oracleTwapDivergence" },
  OracleMaxAge: { unit: "secs", min: 1, max: 3_600 },
  OracleK: { unit: "bps", min: 10_000, max: 30_000 },
  OracleTwapStaleness: { unit: "secs", min: 1, max: 3600 },
  Scr: { unit: "bps", min: 10_500, max: 15_000, note: "must stay <= mcr" },
};
/** Params that live on `MarketOracle` — queue/execute must pass the optional market_oracle account
 * for these (mirrors governance.rs::param_targets_oracle). Case-insensitive: callers hold either the
 * PascalCase IDL variant or Anchor's camelCase decoded key. */
const ORACLE_PARAMS = ["OracleMaxConf", "OracleMaxDeviation", "OracleTwapDivergence",
  "OracleLiqDivergence", "OracleMaxAge", "OracleK", "OracleTwapStaleness"];
export const isOracleParam = (name: string): boolean =>
  ORACLE_PARAMS.some((p) => p.toLowerCase() === name.toLowerCase());
export const GLOBAL_CLAMPS: Record<string, Clamp> = {
  Cut: { unit: "bps", max: 3000 },
  ReserveCap: { unit: "fUSD-native", note: "0 = no accrual; no upper clamp" },
  DrawBase: { unit: "fUSD-native", note: "no upper clamp" },
  DrawK: { unit: "bps", max: 100_000 },
  DrawCeilingShare: { unit: "bps", max: 10_000 },
  DrawDebtShare: { unit: "bps", max: 10_000 },
};

/** Returns a human warning if `value` is outside the documented clamp for `name`, else null. The
 * on-chain handler re-validates and is authoritative — this is a pre-flight guardrail, not the gate. */
export function clampWarning(name: string, value: bigint, table: Record<string, Clamp>): string | null {
  const c = table[name]; if (!c) return null;
  if (c.min !== undefined && value < BigInt(c.min)) return `value ${value} < documented min ${c.min} (${c.unit})`;
  if (c.max !== undefined && value > BigInt(c.max)) return `value ${value} > documented max ${c.max} (${c.unit})`;
  return null;
}

/** Build the instruction; PRINT it as governance-proposal-ready JSON (e.g. for a multisig) unless
 * `send`, in which case sign + submit. */
export async function sendOrPrint(methodBuilder: any, label: string, send: boolean): Promise<void> {
  const ix = await methodBuilder.instruction();
  if (!send) {
    log(`DRY-RUN: ${label}`);
    log("  re-run with --send to submit, or propose this instruction via your governance process (e.g. a Squads multisig vault):");
    console.log(JSON.stringify({
      programId: ix.programId.toBase58(),
      keys: ix.keys.map((k: anchor.web3.AccountMeta) => ({ pubkey: k.pubkey.toBase58(), isSigner: k.isSigner, isWritable: k.isWritable })),
      data: ix.data.toString("base64"),
    }, null, 2));
    return;
  }
  const sig = await methodBuilder.rpc();
  log(`✓ ${label} — submitted (${sig})`);
}
