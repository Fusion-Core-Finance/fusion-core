/**
 * fUSD governance param tool — drives the TIMELOCKED parameter lifecycle:
 *   queue_param_change(param, value) → (wait gov_gate.timelock_secs) → execute_param_change()
 * with cancel_param_change() to withdraw a queued op, plus the GLOBAL (backstop) twin and the
 * guardian de-risk lever. The valid params + their encoding come from the loaded IDL, so the tool
 * always matches the deployed program.
 *
 * SAFETY: every mutating action is DRY-RUN by default — it prints the instruction (Squads-proposal
 * ready) and only signs + submits when you pass --send. queue/cancel must be signed by the gov gate's
 * inbound_authority; execute is permissionless once the timelock elapses; guardian-derisk by the guardian.
 * Use --authority <pubkey> in dry-run to emit an instruction whose signer is your Squads vault PDA.
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=<authority.json> npx ts-node keepers/set-param.ts <cmd> [flags]
 *   status   [--market <mint>]                       show the gate, timelock, queued ops, live values
 *   queue    --param <name> --value <n> --market <mint> [--send] [--authority <pk>]
 *   execute  --nonce <n> [--send]                    apply a queued market op (after its eta)
 *   cancel   --nonce <n> [--send] [--authority <pk>] withdraw a queued market op
 *   queue-global   --param <name> --value <n> [--send] [--authority <pk>]
 *   execute-global --nonce <n> [--send]
 *   cancel-global  --nonce <n> [--send] [--authority <pk>]
 *   guardian-derisk --market <mint> --secs <n> [--send] [--authority <pk>]
 *
 * param names (market): mcr debtCeiling redemptionFee liqGasComp rateLimitCap ccr liqBonus minDebt
 *                       rateAdjustCooldown keeperReward borrowFee badDebtPaydown redemptionBaseRateMax
 *                       oracleMaxConf oracleMaxDeviation oracleTwapDivergence oracleLiqDivergence
 *                       oracleMaxAge oracleK oracleTwapStaleness scr
 * param names (global):  cut reserveCap drawBase drawK drawCeilingShare drawDebtShare
 * (the oracle* params live on MarketOracle — queue/execute auto-include the market_oracle account)
 */
import * as fs from "fs";
import * as anchor from "@coral-xyz/anchor";
import { BN, makeProgram, PublicKey, Pk } from "./common";
import {
  flags, govPdas, timelockPda, gtimelockPda, marketPda, marketOraclePda, resolveVariant, clampWarning,
  MARKET_CLAMPS, GLOBAL_CLAMPS, isOracleParam, sendOrPrint, authorityOf, printUsage, log,
} from "./gov-common";

const SYS = anchor.web3.SystemProgram.programId;
// Read variant names from the IDL FILE — the anchor Program object processes/strips idl.types.
// Lazy + memoized: only queue/queue-global need it, and `help` must work on a build-less clone.
let IDL: any;
const idlVariants = (typeName: string): string[] => {
  if (!IDL) IDL = JSON.parse(fs.readFileSync(`${__dirname}/../target/idl/fusd_core.json`, "utf8"));
  return (IDL.types ?? []).find((t: any) => t.name === typeName)?.type.variants.map((v: any) => v.name) ?? [];
};
const paramName = (decoded: any): string => Object.keys(decoded)[0]; // Anchor decodes an enum to { camelName: {} }

const reqMint = (f: any, name = "market"): Pk => {
  const v = f.get(name); if (!v) throw new Error(`--${name} <collateral mint> is required`);
  return new PublicKey(v);
};
const reqNonce = (f: any): bigint => {
  const v = f.get("nonce"); if (v === undefined) throw new Error("--nonce <n> is required");
  return BigInt(v);
};
const reqValue = (f: any): bigint => {
  const v = f.get("value"); if (v === undefined) throw new Error("--value <n> is required");
  const n = BigInt(v); if (n < 0n) throw new Error("value must be non-negative"); return n;
};

async function status(program: any, pid: Pk, f: any) {
  const g = govPdas(pid);
  const gate: any = await program.account.governanceGate.fetch(g.govGate);
  log(`gov gate ${g.govGate.toBase58()}`);
  log(`  inbound_authority   ${gate.inboundAuthority.toBase58()}`);
  if (!gate.pendingInboundAuthority.equals(PublicKey.default)) log(`  pending_inbound     ${gate.pendingInboundAuthority.toBase58()}`);
  log(`  timelock_secs       ${gate.timelockSecs}`);
  log(`  queue_nonce         ${gate.queueNonce}`);

  const now = Math.floor(Date.now() / 1000);
  const total = Number(gate.queueNonce);
  const start = Math.max(0, total - 64); // bound the scan to the most recent ops
  log(`queued ops (nonce ${start}..${Math.max(start, total - 1)}):`);
  let found = 0;
  const eta = (e: any) => `eta ${e} (${now >= Number(e) ? "READY" : (Number(e) - now) + "s left"})`;
  const nonces = Array.from({ length: total - start }, (_, i) => BigInt(start + i));
  // Two batched getMultipleAccounts calls instead of 2×64 sequential round trips.
  const [mks, gls] = nonces.length
    ? await Promise.all([
        program.account.timelockedParam.fetchMultiple(nonces.map((n) => timelockPda(pid, n))),
        program.account.timelockedGlobalParam.fetchMultiple(nonces.map((n) => gtimelockPda(pid, n))),
      ])
    : [[], []];
  nonces.forEach((nonce, i) => {
    const n = Number(nonce);
    const mk = mks[i], gl = gls[i];
    if (mk) { found++; log(`  #${n} MARKET ${paramName(mk.param)}=${mk.value} ${eta(mk.eta)} market ${mk.market.toBase58().slice(0, 8)}…`); }
    if (gl) { found++; log(`  #${n} GLOBAL ${paramName(gl.param)}=${gl.value} ${eta(gl.eta)}`); }
  });
  if (!found) log("  (none)");

  const mintArg = f.get("market");
  if (mintArg) {
    const coll = new PublicKey(mintArg);
    const m: any = await program.account.market.fetch(marketPda(pid, coll));
    log(`market ${mintArg.slice(0, 8)}… live values:`);
    log(`  mcr_bps ${m.mcrBps}  ccr_bps ${m.ccrBps}  scr_bps ${m.scrBps}  debt_ceiling ${m.debtCeiling}  redemption_fee_bps ${m.redemptionFeeBps}`);
    log(`  liq_gas_comp_bps ${m.liqGasCompBps}  liq_bonus_bps ${m.liqBonusBps}  min_debt ${m.minDebt}  keeper_reward_bps ${m.keeperRewardBps}`);
    log(`  rate_adjust_cooldown_secs ${m.rateAdjustCooldownSecs}  rl_cap ${m.rlCap}  borrow_fee_bps ${m.borrowFeeBps}`);
    log(`  bad_debt_paydown_bps ${m.badDebtPaydownBps}  redemption_base_rate_max_bps ${m.redemptionBaseRateMaxBps}`);
    const o: any = await program.account.marketOracle.fetchNullable(marketOraclePda(pid, coll));
    if (o) {
      log(`market oracle live values:`);
      log(`  max_conf_bps ${o.maxConfBps}  max_deviation_bps ${o.maxDeviationBps}  twap_max_divergence_bps ${o.twapMaxDivergenceBps}  liq_max_divergence_bps ${o.liqMaxDivergenceBps}`);
      log(`  max_age_secs ${o.maxAgeSecs}  k_bps ${o.kBps}  twap_max_staleness_secs ${o.twapMaxStalenessSecs}`);
    }
  }
}

async function main() {
  const cmd = process.argv[2];
  const f = flags(process.argv.slice(3));
  const send = f.has("send");
  if (!cmd || cmd === "help" || cmd === "--help") { printUsage(__filename); return; }

  const { program, pid, me } = makeProgram();
  const g = govPdas(pid);

  switch (cmd) {
    case "status": return status(program, pid, f);

    case "queue": {
      const { name, arg } = resolveVariant(idlVariants("MarketParam"), f.get("param") ?? "");
      const value = reqValue(f); const coll = reqMint(f); const market = marketPda(pid, coll);
      const w = clampWarning(name, value, MARKET_CLAMPS); if (w) log(`⚠ ${name}: ${w}`);
      const gate: any = await program.account.governanceGate.fetch(g.govGate);
      const nonce = BigInt(gate.queueNonce.toString());
      log(`queue MARKET ${name}=${value} → nonce ${nonce}, executable ~+${gate.timelockSecs}s after submit`);
      const b = program.methods.queueParamChange(arg, new BN(value.toString())).accounts({
        authority: authorityOf(f, me, send), govGate: g.govGate, market,
        // oracle-targeting params validate/write MarketOracle — the optional account is required then
        marketOracle: isOracleParam(name) ? marketOraclePda(pid, coll) : null,
        timelockedParam: timelockPda(pid, nonce), systemProgram: SYS,
      });
      return sendOrPrint(b, `queue ${name}=${value} (nonce ${nonce})`, send);
    }

    case "execute": {
      const nonce = reqNonce(f); const tp = timelockPda(pid, nonce);
      const op: any = await program.account.timelockedParam.fetch(tp);
      const now = Math.floor(Date.now() / 1000);
      if (now < Number(op.eta)) log(`⚠ timelock not elapsed — ${Number(op.eta) - now}s remain (tx reverts until then)`);
      const oracleNeeded = isOracleParam(paramName(op.param));
      const m: any = oracleNeeded ? await program.account.market.fetch(op.market) : null;
      const b = program.methods.executeParamChange().accounts({
        executor: me, market: op.market,
        marketOracle: oracleNeeded ? marketOraclePda(pid, m.collateralMint) : null,
        timelockedParam: tp,
      });
      return sendOrPrint(b, `execute nonce ${nonce} (${paramName(op.param)}=${op.value})`, send);
    }

    case "cancel": {
      const nonce = reqNonce(f); const tp = timelockPda(pid, nonce);
      const b = program.methods.cancelParamChange().accounts({ authority: authorityOf(f, me, send), govGate: g.govGate, timelockedParam: tp });
      return sendOrPrint(b, `cancel nonce ${nonce}`, send);
    }

    case "queue-global": {
      const { name, arg } = resolveVariant(idlVariants("GlobalParam"), f.get("param") ?? "");
      const value = reqValue(f);
      const w = clampWarning(name, value, GLOBAL_CLAMPS); if (w) log(`⚠ ${name}: ${w}`);
      const gate: any = await program.account.governanceGate.fetch(g.govGate);
      const nonce = BigInt(gate.queueNonce.toString());
      log(`queue GLOBAL ${name}=${value} → nonce ${nonce} (+${gate.timelockSecs}s)`);
      const b = program.methods.queueGlobalParamChange(arg, new BN(value.toString())).accounts({
        authority: authorityOf(f, me, send), govGate: g.govGate, timelockedParam: gtimelockPda(pid, nonce), systemProgram: SYS,
      });
      return sendOrPrint(b, `queue-global ${name}=${value} (nonce ${nonce})`, send);
    }

    case "execute-global": {
      const nonce = reqNonce(f); const tp = gtimelockPda(pid, nonce);
      const op: any = await program.account.timelockedGlobalParam.fetch(tp);
      const now = Math.floor(Date.now() / 1000);
      if (now < Number(op.eta)) log(`⚠ timelock not elapsed — ${Number(op.eta) - now}s remain`);
      const b = program.methods.executeGlobalParamChange().accounts({ executor: me, backstop: g.backstop, timelockedParam: tp });
      return sendOrPrint(b, `execute-global nonce ${nonce} (${paramName(op.param)}=${op.value})`, send);
    }

    case "cancel-global": {
      const nonce = reqNonce(f); const tp = gtimelockPda(pid, nonce);
      const b = program.methods.cancelGlobalParamChange().accounts({ authority: authorityOf(f, me, send), govGate: g.govGate, timelockedParam: tp });
      return sendOrPrint(b, `cancel-global nonce ${nonce}`, send);
    }

    case "guardian-derisk": {
      const coll = reqMint(f); const secs = BigInt(f.get("secs") ?? "0");
      if (secs <= 0n) throw new Error("--secs <n> (positive) is required");
      const b = program.methods.guardianDerisk(new BN(secs.toString())).accounts({
        guardian: authorityOf(f, me, send), config: g.config, collateralMint: coll, market: marketPda(pid, coll),
      });
      return sendOrPrint(b, `guardian-derisk ${coll.toBase58().slice(0, 6)} ${secs}s`, send);
    }

    default: throw new Error(`unknown command "${cmd}" — run: npx ts-node keepers/set-param.ts help`);
  }
}

if (require.main === module) {
  main().catch((e) => { console.error("ERROR:", e.message || e); process.exit(1); });
}
