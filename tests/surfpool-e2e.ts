/**
 * Tier-2 surfpool integration client — runs fUSD's REAL oracle path against live mainnet
 * (Orca/Pyth/Switchboard) accounts on a surfnet fork. This is the "level 2" test the in-process
 * litesvm harness can't do (it uses fabricated oracle accounts); here the on-chain instructions
 * parse the ACTUAL forked Orca pool + Pyth price feed + Switchboard feed.
 *
 * TWO MODES
 *   DEFAULT (no env)  — the clean, no-cheatcode real-data proof:
 *     1. init_protocol + init_market(WSOL) + init_market_oracle(real Orca + Pyth + Switchboard)
 *     2. sample_twap  -> on-chain CLMM parse of the REAL Orca WSOL/USDC pool          [core win]
 *     3. update_price -> on-chain Pyth parse + aggregate -> Market.spot               [core win]
 *     4. borrow       -> reports FROZEN. A clean static fork CANNOT reach the "Ok" aggregate by
 *                        design: the Switchboard leg is pull-based (stale-on-fork) and the TWAP
 *                        needs a 300s on-chain span. That strictness is a feature, not a bug.
 *
 *   FULL_BORROW=1  — drives a real end-to-end borrow using surfnet CHEATCODES (honest caveat:
 *     this synthesizes oracle FRESHNESS — it warps the clock to span the TWAP window and patches
 *     the frozen Pyth/Switchboard publish-times to "now". The prices stay REAL (the forked Orca
 *     $/SOL, the real Pyth $/SOL, the real Switchboard $64.85 median); only their timestamps are
 *     nudged so the staleness gate passes. The borrow path itself runs unmodified.). Requires a
 *     FRESH surfnet (the oracle is init-only, so the real Switchboard feed must be bound at init).
 *
 * PREREQUISITES
 *   - A surfnet fork with fUSD deployed:  cd <your fusion-core checkout> && surfpool start --network mainnet --yes
 *   - The IDL at target/idl/fusd_core.json (anchor build); a funded keypair at ~/.config/solana/id.json.
 *   - FULL_BORROW also needs @solana/spl-token (`npm i @solana/spl-token`) and Node >= 18 (global fetch).
 *
 * RUN
 *   npx ts-node tests/surfpool-e2e.ts                 # clean Stages 1-3 (works on a running surfnet)
 *   FULL_BORROW=1 npx ts-node tests/surfpool-e2e.ts   # full borrow (restart surfpool first)
 */

import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import * as os from "os";

const { PublicKey, Keypair, SystemProgram, Connection } = anchor.web3;
const BN = anchor.BN;
type PublicKeyT = anchor.web3.PublicKey;

// ---------------------------------------------------------------------------------------------
// Config — real mainnet addresses (forked on demand by surfnet) + the program's known constants.
// ---------------------------------------------------------------------------------------------
const RPC_URL = process.env.RPC_URL || "http://127.0.0.1:8899";
const WALLET_PATH = process.env.WALLET || `${os.homedir()}/.config/solana/id.json`;
const FULL = !!process.env.FULL_BORROW; // opt into the cheatcode-driven borrow

const WSOL = new PublicKey("So11111111111111111111111111111111111111112"); // collateral
const USDC = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"); // quote
const ORCA_WSOL_USDC_POOL = new PublicKey("Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE"); // real Orca Whirlpool
// Pyth SOL/USD price feed id (32 bytes) — bound into the market oracle; the on-chain feed account
// below must carry this same id.
const SOL_USD_FEED_ID_HEX = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
// The persistent, continuously-updated SOL/USD `PriceUpdateV2` account (receiver-owned). Verified on
// mainnet 2026-06-09 (freshest of the receiver's SOL/USD feeds). Override with PYTH_ACCOUNT=<pubkey>.
const PYTH_SOL_USD_ACCOUNT = "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE";
// The real Switchboard On-Demand SOL/USD PullFeed (owner SBondMDrc…, 3208 bytes, name "SOL/USD",
// median ~$64.85). Verified on mainnet 2026-06-09. Bound at init; used by FULL_BORROW. Override SB_FEED.
const SB_SOL_USD = process.env.SB_FEED || "4Hmd6PdjVA9auCoScE12iaBogfwS4ZXQ6VZoBeqanwWW";

const PYTH_RECEIVER = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";       // owner of every PriceUpdateV2
const SB_ONDEMAND = "SBondMDrcV3K4kxZR1HNVT7osZxAHVHgYXL5Ze1oMUv";        // owner of every PullFeedAccountData
const SYSVAR_CLOCK = new PublicKey("SysvarC1ock11111111111111111111111111111111");

const RAY = 10n ** 27n;
const FUSD_DECIMALS = 6;
const COLL_DECIMALS = 9; // WSOL
const TWAP_WINDOW = 300; // twap_window_secs (clamp minimum)

// ---------------------------------------------------------------------------------------------
function loadKeypair(path: string) {
  return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(path, "utf8"))));
}
function feedIdBytes(): Buffer {
  return Buffer.from(SOL_USD_FEED_ID_HEX, "hex");
}
const u128le = (buf: Buffer, off: number): bigint => buf.readBigUInt64LE(off) + (buf.readBigUInt64LE(off + 8) << 64n);
const usdRayToUsd = (p: bigint) => Number(p / 10n ** 21n) / 1e6;       // p / RAY, kept in JS-safe range
const e18ToUsd = (v: bigint) => Number(v / 10n ** 15n) / 1000;         // 1e18-scaled (Switchboard) -> USD
function spotToUsd(spot: anchor.BN): number {
  // spot = usd * 10^FUSD_DECIMALS * RAY / 10^COLL_DECIMALS  (RAY-scaled fUSD-native per native coll unit)
  const p = BigInt(spot.toString());
  const scale = RAY * 10n ** BigInt(FUSD_DECIMALS) / 10n ** BigInt(COLL_DECIMALS); // = RAY * 1e-3
  return Number((p * 1000n) / scale) / 1000;
}

// PDA helpers (seeds from programs/fusd-core/src/constants.rs).
function pda(seeds: (Buffer | PublicKeyT)[], programId: PublicKeyT): PublicKeyT {
  return PublicKey.findProgramAddressSync(seeds.map((s) => (s instanceof PublicKey ? s.toBuffer() : s)), programId)[0];
}

async function trySend(label: string, fn: () => Promise<string>) {
  try {
    const sig = await fn();
    console.log(`  ✓ ${label}  (${sig.slice(0, 16)}…)`);
  } catch (e: any) {
    const msg = e?.message || String(e);
    if (/already in use|custom program error: 0x0\b|account already exists/i.test(msg)) {
      console.log(`  • ${label} — already initialized, skipping`);
    } else {
      throw new Error(`${label} FAILED: ${msg}`);
    }
  }
}

// ---- surfnet cheatcode RPCs (only used by FULL_BORROW) ---------------------------------------
const fetchFn: any = (globalThis as any).fetch;
async function rpc(method: string, params: any[]): Promise<any> {
  if (!fetchFn) throw new Error("global fetch unavailable — FULL_BORROW needs Node >= 18");
  const res = await fetchFn(RPC_URL, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  const j: any = await res.json();
  if (j.error) throw new Error(`${method} RPC error: ${JSON.stringify(j.error)}`);
  return j.result;
}
/** Jump the surfnet Clock sysvar to an absolute unix time (surfnet_timeTravel). NOTE: absoluteTimestamp
 * is in MILLISECONDS (surfpool's internal `updated_at` scale), while the Clock sysvar / Pyth publish_time
 * are in SECONDS — so convert here. We pass seconds in and scale to ms. */
async function warpTo(unixSecs: bigint) {
  await rpc("surfnet_timeTravel", [{ absoluteTimestamp: Number(unixSecs) * 1000 }]);
}
/** Overwrite an account's bytes in place, preserving owner/lamports (surfnet_setAccount). Data is raw
 * hex WITHOUT a 0x prefix — surfnet rejects the prefix ("Invalid character 'x' at position 1"). */
async function setAccountData(conn: any, pubkey: PublicKeyT, newData: Buffer) {
  const info = await conn.getAccountInfo(pubkey);
  await rpc("surfnet_setAccount", [
    pubkey.toBase58(),
    {
      data: newData.toString("hex"),
      owner: info.owner.toBase58(),
      lamports: info.lamports,
      executable: false,
      rentEpoch: 0,
    },
  ]);
}
async function getClock(conn: any): Promise<{ slot: bigint; unixTs: bigint }> {
  const info = await conn.getAccountInfo(SYSVAR_CLOCK);
  // Clock layout: slot u64@0, epoch_start_ts i64@8, epoch u64@16, leader_sched_epoch u64@24, unix_ts i64@32.
  return { slot: info.data.readBigUInt64LE(0), unixTs: info.data.readBigInt64LE(32) };
}

async function main() {
  const conn = new Connection(RPC_URL, "confirmed");
  const wallet = new anchor.Wallet(loadKeypair(WALLET_PATH));
  const provider = new anchor.AnchorProvider(conn, wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);

  const idl = JSON.parse(fs.readFileSync(`${__dirname}/../target/idl/fusd_core.json`, "utf8"));
  // Typed as `any`: the IDL loads at runtime, so the strongly-typed account/method namespaces aren't
  // known at compile time (and an optional account takes `null`).
  const program: any = new anchor.Program(idl as anchor.Idl, provider);
  const pid = program.programId;
  const me = wallet.publicKey;
  console.log(`fUSD program: ${pid.toBase58()}\nwallet:       ${me.toBase58()}\nRPC:          ${RPC_URL}`);
  console.log(`mode:         ${FULL ? "FULL_BORROW (uses surfnet cheatcodes)" : "clean (real-data parse only)"}\n`);

  // PDAs
  const config = pda([Buffer.from("config")], pid);
  const fusdMint = pda([Buffer.from("fusd_mint")], pid);
  const mintAuthority = pda([Buffer.from("mint_authority")], pid);
  const market = pda([Buffer.from("market"), WSOL], pid);
  const collateralVault = pda([Buffer.from("coll_vault"), WSOL], pid);
  const marketOracle = pda([Buffer.from("oracle"), WSOL], pid);
  const dexTwap = pda([Buffer.from("twap"), WSOL], pid);
  const redemptionBitmap = pda([Buffer.from("redeem_bitmap"), WSOL], pid);
  const position = pda([Buffer.from("position"), WSOL, me], pid);
  const pythAccount = new PublicKey(process.env.PYTH_ACCOUNT || PYTH_SOL_USD_ACCOUNT);
  const sbAccount = new PublicKey(SB_SOL_USD);
  const TOKEN = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"); // legacy SPL Token

  // Preflight: `surfpool start` deploys fUSD via its txtx runbook a few seconds AFTER the validator is
  // up. Racing that deploy makes init fail with "This program may not be used for executing instructions".
  // Wait (briefly) for the program account to become executable.
  let deployed = false;
  for (let i = 0; i < 60 && !deployed; i++) {
    deployed = !!(await conn.getAccountInfo(pid))?.executable;
    if (!deployed) {
      if (i === 0) process.stdout.write("waiting for fUSD to finish deploying");
      else process.stdout.write(".");
      await new Promise((r) => setTimeout(r, 1000));
    }
  }
  if (!deployed)
    throw new Error("fUSD program is not executable — surfpool is likely still deploying. Wait for its " +
      "\"Runbook 'deployment' execution completed\" line, then re-run.");
  console.log("✓ fUSD program is deployed + executable.\n");

  // ----------------------------------------------------------------- Stage 1: init
  console.log("── Stage 1: init protocol + WSOL market + oracle (real Orca + Pyth + Switchboard) ──");
  // NOTE: init_protocol is now gated on the program's UPGRADE AUTHORITY — the payer must
  // equal it. The deploying wallet is the upgrade authority by default, so `me` qualifies here. The
  // `program_data` account is auto-resolved by Anchor (the IDL encodes its PDA + the BPF loader), so
  // it does not need to be listed below. If this reverts with 6000 (Unauthorized), the surfnet deploy
  // gave the program a different upgrade authority than the wallet running this script.
  await trySend("init_protocol", () =>
    program.methods.initProtocol({ govAuthority: me, guardian: me }).accounts({
      payer: me, config, mintAuthority, fusdMint, tokenProgram: TOKEN,
      systemProgram: SystemProgram.programId, rent: anchor.web3.SYSVAR_RENT_PUBKEY,
    }).rpc());

  await trySend("init_market(WSOL)", () =>
    program.methods.initMarket({
      mcrBps: 15000, debtCeiling: new BN(1_000_000_000_000), reserveLamports: new BN(0),
      liqGasCompBps: 0, bucketWidthBps: 10, redemptionFeeBps: 0, liqBonusBps: 1000,
    }).accounts({
      authority: me, config, collateralMint: WSOL, market, collateralVault, redemptionBitmap,
      tokenProgram: TOKEN, systemProgram: SystemProgram.programId, rent: anchor.web3.SYSVAR_RENT_PUBKEY,
    }).rpc());

  await trySend("init_market_oracle(Orca + Pyth + Switchboard)", () =>
    program.methods.initMarketOracle({
      pythFeedId: Array.from(feedIdBytes()),
      // Bind the REAL Switchboard On-Demand SOL/USD feed. The oracle is created ONCE (init-only, no
      // governance update path), so FULL_BORROW requires this to be the bound key — i.e. a fresh surfnet.
      switchboardFeed: sbAccount,
      orcaPool: ORCA_WSOL_USDC_POOL,
      raydiumPool: PublicKey.default,
      maxConfBps: 200, maxDeviationBps: 500, twapMaxDivergenceBps: 1000,
      maxAgeSecs: new BN(300), // = MAX_ORACLE_MAX_AGE_SECS (the most lenient the clamp allows)
      kBps: 21200,
      // TWAP clamps: window >= 300s, min_samples >= 3. A clean fork can't satisfy the corridor (would
      // need >=3 samples spanning 5 min of on-chain time) -> aggregate stays frozen, but spot is still
      // set from the fresh Pyth feed (Stage 3). FULL_BORROW spans the window with a clock warp.
      twapWindowSecs: new BN(TWAP_WINDOW), twapMinSamples: 3, twapMaxStalenessSecs: new BN(3600),
      // Args added to InitMarketOracleArgs after this harness was first written (C6 plausibility band,
      // B3 liq-divergence, C1 LST canonical leg) — all default-OFF here (WSOL is a non-LST market).
      // Pass them explicitly rather than relying on Anchor zero-defaulting a missing struct field.
      priceBandLowerRay: new BN(0), priceBandUpperRay: new BN(0), liqMaxDivergenceBps: 0,
      lstStakePool: PublicKey.default,
    }).accounts({
      authority: me, config, collateralMint: WSOL, quoteMint: USDC, market, marketOracle, dexTwap,
      systemProgram: SystemProgram.programId,
    }).rpc());

  // FULL_BORROW needs the oracle bound to the real Switchboard feed (it's init-only). Detect early.
  const oracleAcct: any = await program.account.marketOracle.fetch(marketOracle);
  const boundSb = oracleAcct.switchboardFeed.toBase58();
  let fullReady = FULL;
  if (FULL && boundSb !== sbAccount.toBase58()) {
    fullReady = false;
    console.log(`\n  ⚠ FULL_BORROW: the market oracle is bound to ${boundSb}, not the real Switchboard feed`);
    console.log(`    ${sbAccount.toBase58()}. The oracle is init-only, so RESTART surfpool to re-init with it:`);
    console.log(`      (kill surfpool) ; surfpool start --network mainnet --yes ; FULL_BORROW=1 npx ts-node tests/surfpool-e2e.ts`);
    console.log(`    Falling back to the clean Pyth-only path (mint stays frozen).`);
  }

  // ----------------------------------------------------------------- Stage 2: sample_twap (REAL Orca)
  console.log("\n── Stage 2: sample_twap against the REAL Orca WSOL/USDC pool (on-chain CLMM parse) ──");
  const orcaInfo = await conn.getAccountInfo(ORCA_WSOL_USDC_POOL);
  console.log(`  forked Orca pool owner: ${orcaInfo?.owner.toBase58()} (len ${orcaInfo?.data.length})`);
  for (let i = 0; i < 2; i++) {
    await trySend(`sample_twap #${i + 1}`, () =>
      program.methods.sampleTwap().accounts({
        cranker: me, collateralMint: WSOL, marketOracle, dexTwap, clmmPool: ORCA_WSOL_USDC_POOL,
      }).rpc());
    if (i === 0) await new Promise((r) => setTimeout(r, 2000));
  }
  // Ring layout: disc8 + prices[64]u128 + ts[64]i64 + next u64 + count u64.
  const N = 64, TS_BASE = 8 + N * 16;
  const readRing = (data: Buffer) => {
    const next = Number(data.readBigUInt64LE(TS_BASE + N * 8));
    const count = Number(data.readBigUInt64LE(TS_BASE + N * 8 + 8));
    const lastIdx = (next + N - 1) % N;
    const oldestIdx = (next + N - count) % N;
    return {
      count,
      lastPrice: u128le(data, 8 + lastIdx * 16),
      lastTs: data.readBigInt64LE(TS_BASE + lastIdx * 8),
      oldestTs: data.readBigInt64LE(TS_BASE + oldestIdx * 8),
    };
  };
  let ring = readRing((await conn.getAccountInfo(dexTwap))!.data);
  console.log(`  ✓ DexTwap last sample: ${usdRayToUsd(ring.lastPrice).toFixed(2)} USD/SOL  (${ring.count} samples — on-chain parse of the real pool)`);

  // ------------------------------------- Stage 3a (FULL only): span the TWAP window via a clock warp,
  //                                       then refresh the frozen Pyth + Switchboard publish-times.
  let sbForUpdate: PublicKeyT | null = null;
  if (fullReady) {
    console.log("\n── Stage 3a (FULL_BORROW): warp the clock to span the 300s TWAP window + refresh feeds ──");
    console.log("    (surfnet cheatcodes — synthesizes oracle FRESHNESS; the prices themselves stay real)");

    // Span the window: warp ~TWAP_WINDOW+10s past the CURRENT clock (NOT a sample ts — the ring keeps
    // samples across runs and the clock advances on its own, so an old sample ts would be in the past),
    // then take a 3rd sample. The fresh Stage-2 samples then predate now-window, newest is fresh, and
    // count >= 3 -> twap() returns Some. Use max(clock, newest sample) so the target is strictly forward.
    const clockNow = (await getClock(conn)).unixTs;
    const baseTs = clockNow > ring.lastTs ? clockNow : ring.lastTs;
    const target = baseTs + BigInt(TWAP_WINDOW + 10);
    await warpTo(target);
    await trySend("sample_twap #3 (post-warp)", () =>
      program.methods.sampleTwap().accounts({
        cranker: me, collateralMint: WSOL, marketOracle, dexTwap, clmmPool: ORCA_WSOL_USDC_POOL,
      }).rpc());
    ring = readRing((await conn.getAccountInfo(dexTwap))!.data);
    const now = (await getClock(conn)).unixTs;
    // Stamp the feeds slightly in the FUTURE so they stay within max_age_secs even if the (fast) clock
    // races ahead between this patch and update_price. A future publish_ts is safe: aggregate uses
    // `now.saturating_sub(publish_ts)`, which is 0 (fresh) when publish_ts > now.
    const freshTs = now + 120n;
    console.log(`  ✓ ring spans ${Number(ring.lastTs - ring.oldestTs)}s with ${ring.count} samples; clock now=${now} (window start=${now - BigInt(TWAP_WINDOW)})`);

    // Refresh Pyth: copy-on-read froze publish_time at the fork; the warp made it stale. Patch
    // publish_time/prev_publish_time to `now` IN PLACE (preserves real price/conf/feed_id/Full level).
    // Locate publish_time by finding the feed_id (robust vs a hardcoded offset): PriceFeedMessage =
    // feed_id[32] price:i64 conf:u64 expo:i32 publish_time:i64 prev_publish_time:i64 ...
    const pInfo = await conn.getAccountInfo(pythAccount);
    if (!pInfo || pInfo.owner.toBase58() !== PYTH_RECEIVER) throw new Error("Pyth feed missing / wrong owner on the fork");
    const pBuf = Buffer.from(pInfo.data);
    const fidx = pBuf.indexOf(feedIdBytes());
    if (fidx < 0) throw new Error("feed_id not found in the Pyth account — layout changed; aborting patch");
    const ptOff = fidx + 32 + 8 + 8 + 4; // -> publish_time
    pBuf.writeBigInt64LE(freshTs, ptOff);
    pBuf.writeBigInt64LE(freshTs - 1n, ptOff + 8); // prev_publish_time
    await setAccountData(conn, pythAccount, pBuf);
    console.log(`  ✓ Pyth publish_time -> ${freshTs} (price/conf/feed_id preserved)`);

    // Refresh Switchboard: PullFeedAccountData (3208 bytes) — verify the layout against the real value,
    // then patch last_update_timestamp@2216 to `now`. result.value@2264 (real $) and result.slot@2368
    // (nonzero) already pass fUSD's checks; only freshness needs fixing.
    const sInfo = await conn.getAccountInfo(sbAccount);
    if (!sInfo || sInfo.owner.toBase58() !== SB_ONDEMAND || sInfo.data.length !== 3208)
      throw new Error(`Switchboard feed missing / wrong owner / unexpected size (${sInfo?.data.length}) on the fork`);
    const sBuf = Buffer.from(sInfo.data);
    const sbVal = u128le(sBuf, 2264);   // result.value, i128 1e18-scaled
    const sbSlot = sBuf.readBigUInt64LE(2368);
    if (sbVal < 10n ** 18n || sbVal > 10000n * 10n ** 18n || sbSlot === 0n) {
      console.log(`  ⚠ Switchboard layout sanity check FAILED (value=${sbVal}, slot=${sbSlot}); skipping SB refresh — aggregate will stay frozen.`);
    } else {
      sBuf.writeBigInt64LE(freshTs, 2216); // last_update_timestamp
      await setAccountData(conn, sbAccount, sBuf);
      sbForUpdate = sbAccount;
      console.log(`  ✓ Switchboard median ${e18ToUsd(sbVal).toFixed(2)} USD/SOL (real), slot ${sbSlot}; last_update -> ${freshTs}`);
    }
  }

  // ----------------------------------------------------------------- Stage 3: update_price (REAL Pyth)
  console.log("\n── Stage 3: update_price from the REAL Pyth SOL/USD feed (on-chain parse + aggregate) ──");
  const pythInfo = await conn.getAccountInfo(pythAccount);
  console.log(`  Pyth feed account: ${pythAccount.toBase58()}`);
  console.log(`  forked Pyth owner: ${pythInfo?.owner.toBase58() ?? "NOT FOUND"} (len ${pythInfo?.data.length ?? 0})`);
  await trySend("update_price", () =>
    program.methods.updatePrice().accounts({
      cranker: me, collateralMint: WSOL, market, marketOracle,
      pythPriceUpdate: pythAccount, switchboardFeed: sbForUpdate, dexTwap,
      // C1 LST canonical leg — WSOL is non-LST, so both optional accounts are null (Anchor requires
      // optional accounts to be passed explicitly since the C1 merge added them to UpdatePrice).
      solUsdPythUpdate: null, lstStakePool: null,
    }).rpc());

  const m: any = await program.account.market.fetch(market);
  console.log(`  ✓ Market.spot = ${spotToUsd(m.spot).toFixed(2)} USD/SOL   (derived on-chain from the real Pyth feed)`);
  console.log(`    mint_frozen = ${m.mintFrozen}${fullReady && sbForUpdate ? "  (Pyth + Switchboard + spanning TWAP all validated on-chain)" : ""}`);

  // ----------------------------------------------------------------- Stage 4: borrow
  console.log("\n── Stage 4: borrow ──");
  if (m.mintFrozen) {
    if (!FULL) {
      console.log("  • Mint is FROZEN — and on a clean static fork that is CORRECT, by design. Reaching the");
      console.log("    'Ok' aggregate needs ALL of: fresh Pyth (ok), tight conf (ok), a fresh Switchboard");
      console.log("    feed within 5% of Pyth, and a DEX-TWAP spanning a full 300s on-chain window. The");
      console.log("    Switchboard leg is pull-based (stale-on-fork) and the TWAP span needs time to pass,");
      console.log("    so a clean fork can't satisfy them — that strictness is the oracle gate working.");
      console.log("    Stages 1–3 proved the headline: the on-chain CLMM + Pyth parsers work on REAL mainnet");
      console.log("    accounts (spot set from the live feed). For a real borrow, re-run with FULL_BORROW=1");
      console.log("    on a fresh surfnet (it warps the clock + refreshes the feeds via surfnet cheatcodes).");
    } else {
      console.log("  • Mint still FROZEN after the FULL_BORROW prep — see the warnings above (likely a stale");
      console.log("    bound Switchboard feed: restart surfpool so init binds the real one, then re-run).");
    }
    return;
  }
  console.log("  Mint is UNFROZEN — wrapping SOL→WSOL, opening a position, depositing, borrowing $100…");
  let splToken: any;
  try { splToken = require("@solana/spl-token"); }
  catch { console.log("  ⚠ borrow needs @solana/spl-token (npm i @solana/spl-token). Skipping."); return; }

  const collAta = splToken.getAssociatedTokenAddressSync(WSOL, me);
  const fusdAta = splToken.getAssociatedTokenAddressSync(fusdMint, me);
  const depositLamports = 5_000_000_000; // 5 WSOL of collateral

  const wrapTx = new anchor.web3.Transaction();
  if (!(await conn.getAccountInfo(collAta)))
    wrapTx.add(splToken.createAssociatedTokenAccountInstruction(me, collAta, me, WSOL));
  wrapTx.add(SystemProgram.transfer({ fromPubkey: me, toPubkey: collAta, lamports: depositLamports }));
  wrapTx.add(splToken.createSyncNativeInstruction(collAta));
  if (!(await conn.getAccountInfo(fusdAta)))
    wrapTx.add(splToken.createAssociatedTokenAccountInstruction(me, fusdAta, me, fusdMint));
  await provider.sendAndConfirm(wrapTx);
  console.log("  ✓ wrapped 5 WSOL + ensured fUSD ATA");

  await trySend("open_position(5% rate)", () =>
    program.methods.openPosition({ userRateBps: 500 }).accounts({
      owner: me, collateralMint: WSOL, market, position, systemProgram: SystemProgram.programId,
    }).rpc());
  await trySend("deposit(5 WSOL)", () =>
    program.methods.deposit(new BN(depositLamports)).accounts({
      owner: me, collateralMint: WSOL, market, position, ownerCollateralAta: collAta,
      collateralVault, redemptionBitmap, tokenProgram: TOKEN, systemProgram: SystemProgram.programId,
    }).rpc());
  await trySend("borrow($100 fUSD)", () =>
    program.methods.borrow(new BN(100_000_000)).accounts({
      owner: me, collateralMint: WSOL, market, position, fusdMint, mintAuthority,
      ownerFusdAta: fusdAta, redemptionBitmap, tokenProgram: TOKEN,
    }).rpc());

  const fusdBal = await conn.getTokenAccountBalance(fusdAta);
  const pos: any = await program.account.position.fetch(position);
  console.log(`  ✓ borrowed — fUSD balance ${fusdBal.value.uiAmount}, recorded_debt ${pos.recordedDebt.toString()}`);
  console.log("\n🎉 Full lifecycle on a mainnet fork: real Orca + real Pyth + real Switchboard -> spot -> WSOL-collateralized fUSD borrow.");
}

main().then(() => console.log("\ndone.")).catch((e) => { console.error("\nERROR:", e.message || e); process.exit(1); });
