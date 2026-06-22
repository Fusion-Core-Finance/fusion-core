/**
 * Shared helpers for the governance client scripts (set-param.ts, keys-sync.ts) — a tiny flag parser,
 * the governance PDAs, the param enum/clamp tables, and the safety-first "build then dry-run unless
 * --send" submitter. These scripts MUTATE protocol params + authority, so the default is to PRINT the
 * instruction (Squads-proposal-ready) and only sign+submit on an explicit --send.
 */
import * as anchor from "@coral-xyz/anchor";
import { PublicKey, Pk, pda, seed, log } from "./common";

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

export function u64le(n: bigint): Buffer { const b = Buffer.alloc(8); b.writeBigUInt64LE(n); return b; }
export const govPdas = (pid: Pk) => ({
  config: pda([seed("config")], pid),
  govGate: pda([seed("gov_gate")], pid),
  backstop: pda([seed("backstop")], pid),
});
export const timelockPda = (pid: Pk, nonce: bigint): Pk => pda([seed("timelock"), u64le(nonce)], pid);
export const gtimelockPda = (pid: Pk, nonce: bigint): Pk => pda([seed("gtimelock"), u64le(nonce)], pid);
export const marketPda = (pid: Pk, coll: Pk): Pk => pda([seed("market"), coll], pid);

/** PascalCase IDL variant → the camelCase key Anchor expects as the enum arg, e.g. "LiqBonus" → {liqBonus:{}}. */
export const camel = (s: string): string => s.charAt(0).toLowerCase() + s.slice(1);

/** Match a user-supplied param name (case-insensitive) against the IDL's variant list. */
export function resolveVariant(idlVariants: string[], input: string): { name: string; arg: any } {
  const hit = idlVariants.find((v) => camel(v).toLowerCase() === input.toLowerCase() || v.toLowerCase() === input.toLowerCase());
  if (!hit) throw new Error(`unknown param "${input}". valid: ${idlVariants.map(camel).join(", ")}`);
  return { name: hit, arg: { [camel(hit)]: {} } };
}

export interface Clamp { unit: string; min?: number; max?: number; note?: string; }
export const MARKET_CLAMPS: Record<string, Clamp> = {
  Mcr: { unit: "bps", min: 10000, max: 30000 },
  DebtCeiling: { unit: "fUSD-native", note: "0 pauses new debt; no upper clamp" },
  RedemptionFee: { unit: "bps", min: 0, max: 500 },
  LiqGasComp: { unit: "bps", min: 0, max: 1000 },
  RateLimitCap: { unit: "fUSD-native/window", note: "0 disables; no upper clamp" },
  Ccr: { unit: "bps", note: "0 disables; otherwise [10000, 30000]" },
  LiqBonus: { unit: "bps", min: 0, max: 2000, note: "0 = collar off (seize all)" },
  MinDebt: { unit: "fUSD-native", min: 0, max: 10_000_000_000, note: "0 disables" },
  RateAdjustCooldown: { unit: "secs", min: 0, max: 2_592_000, note: "0 disables" },
  KeeperReward: { unit: "bps", min: 0, max: 1000, note: "0 disables" },
  BorrowFee: { unit: "bps", note: "present in some builds; clamp enforced on-chain" },
};
export const GLOBAL_CLAMPS: Record<string, Clamp> = {
  Cut: { unit: "bps", min: 0, max: 3000 },
  ReserveCap: { unit: "fUSD-native", note: "0 = no accrual; no upper clamp" },
  DrawBase: { unit: "fUSD-native", note: "no upper clamp" },
  DrawK: { unit: "bps", min: 0, max: 100_000 },
  DrawCeilingShare: { unit: "bps", min: 0, max: 10_000 },
  DrawDebtShare: { unit: "bps", min: 0, max: 10_000 },
};

/** Returns a human warning if `value` is outside the documented clamp for `name`, else null. The
 * on-chain handler re-validates and is authoritative — this is a pre-flight guardrail, not the gate. */
export function clampWarning(name: string, value: bigint, table: Record<string, Clamp>): string | null {
  const c = table[name]; if (!c) return null;
  if (c.min !== undefined && value < BigInt(c.min)) return `value ${value} < documented min ${c.min} (${c.unit})`;
  if (c.max !== undefined && value > BigInt(c.max)) return `value ${value} > documented max ${c.max} (${c.unit})`;
  return null;
}

/** Build the instruction; PRINT it (Squads-proposal-ready) unless `send`, in which case sign + submit. */
export async function sendOrPrint(methodBuilder: any, label: string, send: boolean): Promise<void> {
  const ix = await methodBuilder.instruction();
  if (!send) {
    log(`DRY-RUN: ${label}`);
    log("  re-run with --send to submit, or propose this instruction via the Squads vault:");
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
