/**
 * Tier-2 surfpool LIFECYCLE harness — drives the full solvency + peg-defense lifecycle of fUSD
 * against REAL mainnet accounts on a surfnet fork. Where surfpool-e2e.ts stops at the first borrow,
 * this continues through the Reactor Pool, a real liquidation (5-tier waterfall), a redemption
 * (bitmap bucket ordering), and refresh_market interest -> buffer. It is the fork-level proof that
 * the solvency engine works on production account layouts, and the reference flow the eventual
 * liquidator/redeemer keeper bots will reuse.
 *
 * THREE ACTORS (single payer; B/C only sign as position owners — A funds everything):
 *   A = the surfnet default keypair  -> payer + RP depositor + liquidator + redeemer (high rate, very safe)
 *   B = fresh keypair                -> leveraged borrower near MCR (mid rate) -> the liquidation victim
 *   C = fresh keypair                -> low-rate borrower (safe)               -> the redemption target
 *
 * FRESHNESS: like surfpool-e2e.ts FULL_BORROW, this uses surfnet cheatcodes to (a) warp the clock to
 * span the 300s TWAP window and (b) patch the frozen Pyth/Switchboard publish-times to "now". The
 * PRICES stay real (forked Orca/Pyth/Switchboard); only timestamps are nudged so staleness gates pass.
 * The liquidation makes B underwater by patching the Pyth PRICE down ~35% (a cheatcode price move,
 * the only synthesized value), then re-running update_price so the on-chain aggregate re-prices.
 *
 * PREREQUISITES
 *   - surfnet fork with fUSD deployed:  surfpool start --network mainnet --no-tui -y
 *   - target/idl/fusd_core.json (anchor build); a funded keypair at ~/.config/solana/id.json
 *   - npm i @solana/spl-token ; Node >= 18 (global fetch for the cheatcode RPCs)
 *
 * RUN
 *   npx ts-node tests/surfpool-lifecycle.ts
 */

import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import * as os from "os";

const { PublicKey, Keypair, SystemProgram, Connection } = anchor.web3;
const BN = anchor.BN;
type PublicKeyT = anchor.web3.PublicKey;

const RPC_URL = process.env.RPC_URL || "http://127.0.0.1:8899";
const WALLET_PATH = process.env.WALLET || `${os.homedir()}/.config/solana/id.json`;

// Real mainnet addresses (forked on demand by surfnet).
const WSOL = new PublicKey("So11111111111111111111111111111111111111112");
const USDC = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const ORCA_WSOL_USDC_POOL = new PublicKey("Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE");
const SOL_USD_FEED_ID_HEX = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
const PYTH_SOL_USD_ACCOUNT = process.env.PYTH_ACCOUNT || "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE";
const SB_SOL_USD = process.env.SB_FEED || "4Hmd6PdjVA9auCoScE12iaBogfwS4ZXQ6VZoBeqanwWW";
const PYTH_RECEIVER = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";
const SB_ONDEMAND = "SBondMDrcV3K4kxZR1HNVT7osZxAHVHgYXL5Ze1oMUv";
const SYSVAR_CLOCK = new PublicKey("SysvarC1ock11111111111111111111111111111111");
const TOKEN = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

const RAY = 10n ** 27n;
const FUSD_DECIMALS = 6;
const COLL_DECIMALS = 9; // WSOL
const TWAP_WINDOW = 300;
const MCR_BPS = 15000;
const usd = (d: number) => new BN(Math.round(d * 1e6)); // fUSD-native
const SOL = (n: number) => Math.round(n * 1e9); // lamports

// ---- helpers (mirrored from surfpool-e2e.ts) ------------------------------------------------
function loadKeypair(path: string) {
  return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(path, "utf8"))));
}
const feedIdBytes = (): Buffer => Buffer.from(SOL_USD_FEED_ID_HEX, "hex");
const u128le = (b: Buffer, o: number): bigint => b.readBigUInt64LE(o) + (b.readBigUInt64LE(o + 8) << 64n);
function spotToUsd(spot: anchor.BN): number {
  const p = BigInt(spot.toString());
  const scale = (RAY * 10n ** BigInt(FUSD_DECIMALS)) / 10n ** BigInt(COLL_DECIMALS);
  return Number((p * 1000n) / scale) / 1000;
}
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
const fetchFn: any = (globalThis as any).fetch;
async function rpc(method: string, params: any[]): Promise<any> {
  const res = await fetchFn(RPC_URL, {
    method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  const j: any = await res.json();
  if (j.error) throw new Error(`${method} RPC error: ${JSON.stringify(j.error)}`);
  return j.result;
}
async function warpTo(unixSecs: bigint) {
  await rpc("surfnet_timeTravel", [{ absoluteTimestamp: Number(unixSecs) * 1000 }]);
}
async function setAccountData(conn: any, pubkey: PublicKeyT, newData: Buffer) {
  const info = await conn.getAccountInfo(pubkey);
  await rpc("surfnet_setAccount", [pubkey.toBase58(), {
    data: newData.toString("hex"), owner: info.owner.toBase58(),
    lamports: info.lamports, executable: false, rentEpoch: 0,
  }]);
}
async function getClock(conn: any): Promise<bigint> {
  const info = await conn.getAccountInfo(SYSVAR_CLOCK);
  return info.data.readBigInt64LE(32); // unix_timestamp
}
function assert(cond: boolean, msg: string) {
  if (!cond) throw new Error(`ASSERT FAILED: ${msg}`);
  console.log(`    ✓ ${msg}`);
}

async function main() {
  const conn = new Connection(RPC_URL, "confirmed");
  const A = loadKeypair(WALLET_PATH);
  const wallet = new anchor.Wallet(A);
  const provider = new anchor.AnchorProvider(conn, wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);
  const idl = JSON.parse(fs.readFileSync(`${__dirname}/../target/idl/fusd_core.json`, "utf8"));
  const program: any = new anchor.Program(idl as anchor.Idl, provider);
  const pid = program.programId;
  const me = A.publicKey;
  const splToken = require("@solana/spl-token");

  // Fresh victim (B) and redemption-target (C). A pays all fees + funds their collateral.
  const B = Keypair.generate();
  const C = Keypair.generate();
  console.log(`fUSD: ${pid.toBase58()}\nA(payer): ${me.toBase58()}\nB(victim): ${B.publicKey.toBase58()}\nC(redeem target): ${C.publicKey.toBase58()}\n`);

  // PDAs
  const config = pda([Buffer.from("config")], pid);
  const fusdMint = pda([Buffer.from("fusd_mint")], pid);
  const mintAuthority = pda([Buffer.from("mint_authority")], pid);
  const market = pda([Buffer.from("market"), WSOL], pid);
  const collVault = pda([Buffer.from("coll_vault"), WSOL], pid);
  const marketOracle = pda([Buffer.from("oracle"), WSOL], pid);
  const dexTwap = pda([Buffer.from("twap"), WSOL], pid);
  const bitmap = pda([Buffer.from("redeem_bitmap"), WSOL], pid);
  const reactorPool = pda([Buffer.from("reactor"), WSOL], pid);
  const ess = pda([Buffer.from("ess"), WSOL], pid);
  const reactorFusdVault = pda([Buffer.from("reactor_fusd"), WSOL], pid);
  const reactorCollVault = pda([Buffer.from("reactor_coll"), WSOL], pid);
  const buffer = pda([Buffer.from("buffer"), WSOL], pid);
  const bufferFusdVault = pda([Buffer.from("buffer_fusd"), WSOL], pid);
  const positionOf = (owner: PublicKeyT) => pda([Buffer.from("position"), WSOL, owner], pid);
  const reactorDepOf = (owner: PublicKeyT) => pda([Buffer.from("reactor_dep"), WSOL, owner], pid);
  const pythAccount = new PublicKey(PYTH_SOL_USD_ACCOUNT);
  const sbAccount = new PublicKey(SB_SOL_USD);
  const ata = (mint: PublicKeyT, owner: PublicKeyT) => splToken.getAssociatedTokenAddressSync(mint, owner, true);

  const tokenBal = async (acc: PublicKeyT): Promise<bigint> => {
    try { return BigInt((await conn.getTokenAccountBalance(acc)).value.amount); } catch { return 0n; }
  };
  const readMarket = () => program.account.market.fetch(market);
  const readPos = (owner: PublicKeyT) => program.account.position.fetch(positionOf(owner));
  const readRP = () => program.account.reactorPool.fetch(reactorPool);
  const mintSupply = async (): Promise<bigint> => BigInt((await conn.getTokenSupply(fusdMint)).value.amount);
  const supplyInvariant = async (label: string) => {
    const m = await readMarket();
    const circ = await mintSupply();
    const rhs = BigInt(m.aggRecordedDebt.toString()) - BigInt(m.unmintedInterest.toString()) + BigInt(m.badDebt.toString());
    assert(circ === rhs, `supply identity (${label}): circulating ${circ} == agg - unminted + bad ${rhs}`);
  };

  // Wait for the program to finish deploying (surfpool deploys a few seconds after the validator is up).
  for (let i = 0; i < 60; i++) {
    if ((await conn.getAccountInfo(pid))?.executable) break;
    if (i === 0) process.stdout.write("waiting for fUSD to deploy");
    else process.stdout.write(".");
    await new Promise((r) => setTimeout(r, 1000));
  }
  if (!(await conn.getAccountInfo(pid))?.executable) throw new Error("fUSD not executable — surfpool still deploying.");
  console.log("✓ fUSD deployed.\n");

  // ── Stage 1: bootstrap protocol + WSOL market + oracle + reactor pool + insurance buffer ──
  console.log("── Stage 1: bootstrap (protocol, market, oracle, reactor pool, insurance buffer) ──");
  await trySend("init_protocol", () =>
    program.methods.initProtocol({ govAuthority: me, guardian: me }).accounts({
      payer: me, config, mintAuthority, fusdMint, tokenProgram: TOKEN,
      systemProgram: SystemProgram.programId, rent: anchor.web3.SYSVAR_RENT_PUBKEY,
    }).rpc());
  await trySend("init_market(WSOL)", () =>
    program.methods.initMarket({
      mcrBps: MCR_BPS, debtCeiling: new BN(1_000_000_000_000), reserveLamports: new BN(0),
      liqGasCompBps: 50, bucketWidthBps: 10, redemptionFeeBps: 0, liqBonusBps: 1000,
    }).accounts({
      authority: me, config, collateralMint: WSOL, market, collateralVault: collVault, redemptionBitmap: bitmap,
      tokenProgram: TOKEN, systemProgram: SystemProgram.programId, rent: anchor.web3.SYSVAR_RENT_PUBKEY,
    }).rpc());
  await trySend("init_market_oracle", () =>
    program.methods.initMarketOracle({
      pythFeedId: Array.from(feedIdBytes()), switchboardFeed: sbAccount,
      orcaPool: ORCA_WSOL_USDC_POOL, raydiumPool: PublicKey.default,
      maxConfBps: 200, maxDeviationBps: 500, twapMaxDivergenceBps: 1000, maxAgeSecs: new BN(300), kBps: 21200,
      twapWindowSecs: new BN(TWAP_WINDOW), twapMinSamples: 3, twapMaxStalenessSecs: new BN(3600),
    }).accounts({
      authority: me, config, collateralMint: WSOL, quoteMint: USDC, market, marketOracle, dexTwap,
      systemProgram: SystemProgram.programId,
    }).rpc());
  await trySend("init_reactor_pool", () =>
    program.methods.initReactorPool().accounts({
      authority: me, config, collateralMint: WSOL, fusdMint, market, reactorPool,
      epochToScaleToSum: ess, reactorFusdVault, reactorCollVault,
      tokenProgram: TOKEN, systemProgram: SystemProgram.programId, rent: anchor.web3.SYSVAR_RENT_PUBKEY,
    }).rpc());
  await trySend("init_insurance_buffer", () =>
    program.methods.initInsuranceBuffer().accounts({
      authority: me, config, collateralMint: WSOL, fusdMint, market, insuranceBuffer: buffer, bufferFusdVault,
      tokenProgram: TOKEN, systemProgram: SystemProgram.programId, rent: anchor.web3.SYSVAR_RENT_PUBKEY,
    }).rpc());

  // Oracle must be bound to the real Switchboard feed (init-only) for the agreeing aggregate.
  const oAcct: any = await program.account.marketOracle.fetch(marketOracle);
  if (oAcct.switchboardFeed.toBase58() !== sbAccount.toBase58())
    throw new Error(`oracle bound to ${oAcct.switchboardFeed.toBase58()}, not the real SB feed — RESTART surfpool for a fresh init.`);

  // ── Stage 2: warm the TWAP, warp to span the window, freshen feeds, update_price -> unfrozen spot ──
  console.log("\n── Stage 2: warm TWAP + freshen real feeds + update_price ──");
  // sample_twap enforces an anti-flood min interval of ceil(window/(N-1)) and twap() needs >=3 samples
  // whose OLDEST predates now-window. Warp the clock between samples (don't rely on real sleeps) so the
  // gaps clear the min interval and the oldest sample spans the full window.
  const doSample = (n: number) =>
    trySend(`sample_twap #${n}`, () =>
      program.methods.sampleTwap().accounts({ cranker: me, collateralMint: WSOL, marketOracle, dexTwap, clmmPool: ORCA_WSOL_USDC_POOL }).rpc());
  const t0 = await getClock(conn);
  await doSample(1);
  await warpTo(t0 + 10n); await doSample(2);
  await warpTo(t0 + 20n); await doSample(3);
  await warpTo(t0 + 20n + BigInt(TWAP_WINDOW + 15)); await doSample(4); // newest; oldest (t0) now predates now-window

  // freshenFeeds: patch Pyth publish_time + SB last_update to `now+120`, optionally scale the Pyth price.
  const freshenFeeds = async (priceFactor = 1) => {
    const now = (await getClock(conn)) + 120n;
    const pInfo = await conn.getAccountInfo(pythAccount);
    if (!pInfo || pInfo.owner.toBase58() !== PYTH_RECEIVER) throw new Error("Pyth feed missing/wrong owner on fork");
    const pBuf = Buffer.from(pInfo.data);
    const fidx = pBuf.indexOf(feedIdBytes());
    if (fidx < 0) throw new Error("Pyth feed_id not found — layout changed");
    if (priceFactor !== 1) {
      const priceOff = fidx + 32; // PriceFeedMessage.price : i64
      const cur = pBuf.readBigInt64LE(priceOff);
      pBuf.writeBigInt64LE(BigInt(Math.round(Number(cur) * priceFactor)), priceOff);
    }
    const ptOff = fidx + 32 + 8 + 8 + 4; // -> publish_time
    pBuf.writeBigInt64LE(now, ptOff);
    pBuf.writeBigInt64LE(now - 1n, ptOff + 8);
    await setAccountData(conn, pythAccount, pBuf);

    const sInfo = await conn.getAccountInfo(sbAccount);
    if (sInfo && sInfo.owner.toBase58() === SB_ONDEMAND && sInfo.data.length === 3208) {
      const sBuf = Buffer.from(sInfo.data);
      sBuf.writeBigInt64LE(now, 2216); // last_update_timestamp
      await setAccountData(conn, sbAccount, sBuf);
    }
  };
  const crank = async (label: string, sb: PublicKeyT | null = sbAccount) =>
    trySend(label, () =>
      program.methods.updatePrice().accounts({
        cranker: me, collateralMint: WSOL, market, marketOracle, pythPriceUpdate: pythAccount, switchboardFeed: sb, dexTwap,
        // C1 LST canonical leg — WSOL is a non-LST market, so both are null (optional accounts must be passed explicitly).
        solUsdPythUpdate: null, lstStakePool: null,
      }).rpc());

  await freshenFeeds();
  await crank("update_price");
  let m: any = await readMarket();
  const P = spotToUsd(m.spot);
  console.log(`  Market.spot = $${P.toFixed(2)}/SOL, mint_frozen=${m.mintFrozen}`);
  if (m.mintFrozen) throw new Error("mint still frozen after freshen — restart surfpool (stale bound SB feed).");

  // ── Stage 3/4: borrows. A safe+high-rate, B leveraged~MCR+mid-rate, C low-rate+safe. ──
  console.log("\n── Stage 3: open + deposit + borrow for A, B, C ──");
  // Fund a collateral ATA owned by `owner` with `lamports` of WSOL (A pays), and ensure an fUSD ATA.
  const setup = async (owner: PublicKeyT, lamports: number) => {
    const cAta = ata(WSOL, owner), fAta = ata(fusdMint, owner);
    const tx = new anchor.web3.Transaction();
    // B/C pay their own position-account rent (open_position uses `payer = owner`), so fund their
    // native SOL too — A only being the tx fee payer is not enough for the owner-paid init CPIs.
    if (!owner.equals(me)) tx.add(SystemProgram.transfer({ fromPubkey: me, toPubkey: owner, lamports: SOL(0.05) }));
    if (!(await conn.getAccountInfo(cAta))) tx.add(splToken.createAssociatedTokenAccountInstruction(me, cAta, owner, WSOL));
    tx.add(SystemProgram.transfer({ fromPubkey: me, toPubkey: cAta, lamports }));
    tx.add(splToken.createSyncNativeInstruction(cAta));
    if (!(await conn.getAccountInfo(fAta))) tx.add(splToken.createAssociatedTokenAccountInstruction(me, fAta, owner, fusdMint));
    await provider.sendAndConfirm(tx);
    return { cAta, fAta };
  };
  // size a borrow as `frac` of max-at-MCR for `depositSol` of collateral at the live price P.
  const sizeBorrow = (depositSol: number, frac: number) => Math.floor((depositSol * P * 1e4 / MCR_BPS) * frac);

  const open = async (kp: any, depositSol: number, rateBps: number, borrowUsd: number) => {
    const owner = kp.publicKey;
    const { cAta, fAta } = await setup(owner, SOL(depositSol));
    const position = positionOf(owner);
    const signers = owner.equals(me) ? [] : [kp];
    await trySend(`open_position(${owner.toBase58().slice(0, 4)}.. @${rateBps}bps)`, () =>
      program.methods.openPosition({ userRateBps: rateBps }).accounts({ owner, collateralMint: WSOL, market, position, systemProgram: SystemProgram.programId }).signers(signers).rpc());
    await trySend(`deposit(${depositSol} WSOL)`, () =>
      program.methods.deposit(new BN(SOL(depositSol))).accounts({ owner, collateralMint: WSOL, market, position, ownerCollateralAta: cAta, collateralVault: collVault, redemptionBitmap: bitmap, tokenProgram: TOKEN, systemProgram: SystemProgram.programId }).signers(signers).rpc());
    await trySend(`borrow($${borrowUsd})`, () =>
      program.methods.borrow(usd(borrowUsd)).accounts({ owner, collateralMint: WSOL, market, position, fusdMint, mintAuthority, ownerFusdAta: fAta, redemptionBitmap: bitmap, tokenProgram: TOKEN }).signers(signers).rpc());
    return { owner, position, fAta, cAta };
  };

  const aBorrow = sizeBorrow(5, 0.30);              // A: 5 SOL, 30% of max -> very safe
  const bBorrow = sizeBorrow(1, 0.88);              // B: 1 SOL, 88% of max -> near MCR (will go underwater)
  const cBorrow = sizeBorrow(2, 0.30);              // C: 2 SOL, safe
  const a = await open(A, 5, 2000, aBorrow);        // high rate
  const b = await open(B, 1, 1000, bBorrow);        // mid rate -> victim
  const c = await open(C, 2, 50, cBorrow);          // lowest rate -> redemption target
  console.log(`  borrows: A $${aBorrow} (rate 20%), B $${bBorrow} (rate 10%), C $${cBorrow} (rate 0.5%)`);
  await supplyInvariant("after borrows");

  // ── Stage 5: A provides fUSD to the Reactor Pool (the liquidation first-loss buffer) ──
  console.log("\n── Stage 5: Reactor Pool — A provides fUSD ──");
  const rpProvide = Math.floor(bBorrow * 1.5); // enough to absorb B's full debt at offset
  await trySend("open_reactor_deposit(A)", () =>
    program.methods.openReactorDeposit().accounts({ owner: me, collateralMint: WSOL, reactorPool, reactorDeposit: reactorDepOf(me), systemProgram: SystemProgram.programId }).rpc());
  await trySend(`provide_to_reactor($${rpProvide})`, () =>
    program.methods.provideToReactor(usd(rpProvide)).accounts({ owner: me, collateralMint: WSOL, fusdMint, reactorPool, epochToScaleToSum: ess, reactorDeposit: reactorDepOf(me), ownerFusdAta: a.fAta, reactorFusdVault, tokenProgram: TOKEN }).rpc());
  assert((await tokenBal(reactorFusdVault)) === BigInt(usd(rpProvide).toString()), `RP fUSD vault holds $${rpProvide}`);
  await supplyInvariant("after RP provide");

  // ── Stage 6: drop the Pyth price ~35% -> B underwater -> A liquidates B (RP-offset waterfall) ──
  console.log("\n── Stage 6: price drop -> liquidate B ──");
  await freshenFeeds(0.65); // patch Pyth PRICE * 0.65 (real value moved by cheatcode) + keep fresh
  await crank("update_price (post-drop)", null); // SB now diverges -> mode frozen, but spot/debt_spot recommit off Pyth
  m = await readMarket();
  console.log(`  Market.spot now $${spotToUsd(m.spot).toFixed(2)}/SOL (debt_spot drives liquidation)`);

  // SETUP_ONLY: stop here, leaving the fork bot-actionable for tests/surfpool/run-bot-smoke.sh —
  // B is underwater + the price is fresh (liquidator target) and C stays healthy in the lowest rate
  // bucket (redeemer target), both at this one post-drop price (no restore needed between the bots).
  if (process.env.SETUP_ONLY) {
    const bHealth = await readPos(B.publicKey);
    console.log(`\n✓ SETUP_ONLY: B debt ${BigInt(bHealth.recordedDebt.toString())} on ${BigInt(bHealth.ink.toString())} lamports @ MCR ${MCR_BPS}bps — underwater, ready to liquidate. C left in the lowest bucket. Stopping before liquidate/redeem.`);
    return;
  }

  const rpFusdBefore = await tokenBal(reactorFusdVault);
  const bDebtBefore = BigInt((await readPos(B.publicKey)).recordedDebt.toString());
  await trySend("liquidate(B)", () =>
    program.methods.liquidate().accounts({
      liquidator: me, collateralMint: WSOL, market, position: b.position, reactorPool, epochToScaleToSum: ess,
      marketCollVault: collVault, reactorFusdVault, reactorCollVault, fusdMint, liquidatorCollateralAta: a.cAta,
      redemptionBitmap: bitmap, insuranceBuffer: buffer, bufferFusdVault, backstop: null, backstopFusdVault: null,
      tokenProgram: TOKEN,
    }).rpc());
  const bPos: any = await readPos(B.publicKey);
  assert(BigInt(bPos.recordedDebt.toString()) === 0n, "B debt cleared to 0");
  assert(BigInt(bPos.ink.toString()) === 0n, "B collateral fully seized");
  const rpFusdAfter = await tokenBal(reactorFusdVault);
  assert(rpFusdBefore - rpFusdAfter === bDebtBefore, `RP fUSD vault drained by B's debt ($${Number(bDebtBefore) / 1e6})`);
  const seizedColl = await tokenBal(reactorCollVault);
  assert(seizedColl > 0n, "RP collateral vault received seized WSOL");
  await supplyInvariant("after liquidation");
  // A (the sole RP depositor) claims the seized collateral. The Liquity-style P/S scale math can leave
  // sub-unit dust in the vault, so assert the meaningful property: the depositor realizes ~all the gain.
  const aCollPreClaim = await tokenBal(a.cAta);
  await trySend("claim_reactor_gains(A)", () =>
    program.methods.claimReactorGains().accounts({ owner: me, collateralMint: WSOL, reactorPool, epochToScaleToSum: ess, reactorDeposit: reactorDepOf(me), reactorCollVault, ownerCollateralAta: a.cAta, tokenProgram: TOKEN }).rpc());
  const aGain = (await tokenBal(a.cAta)) - aCollPreClaim;
  assert(aGain >= (seizedColl * 90n) / 100n, `sole depositor A realized ~all seized collateral (${aGain}/${seizedColl} lamports; dust may remain)`);

  // ── Stage 7: restore the price -> A redeems fUSD against C (the lowest-rate bucket) ──
  console.log("\n── Stage 7: restore price -> redeem against C (lowest bucket) ──");
  await freshenFeeds(1 / 0.65); // undo the drop (back to the real forked price)
  await crank("update_price (restored)");
  const redeemAmt = Math.max(1, Math.floor(cBorrow * 0.3));
  const cDebtBefore = BigInt((await readPos(C.publicKey)).recordedDebt.toString());
  const aCollBefore = await tokenBal(a.cAta);
  const circBefore = await mintSupply();
  await trySend(`redeem($${redeemAmt}) vs C`, () =>
    program.methods.redeem(usd(redeemAmt)).accounts({
      redeemer: me, collateralMint: WSOL, market, redemptionBitmap: bitmap, fusdMint, marketCollVault: collVault,
      redeemerFusdAta: a.fAta, redeemerCollateralAta: a.cAta, tokenProgram: TOKEN,
    }).remainingAccounts([{ pubkey: c.position, isWritable: true, isSigner: false }]).rpc());
  const cDebtAfter = BigInt((await readPos(C.publicKey)).recordedDebt.toString());
  assert(cDebtAfter < cDebtBefore, `C debt reduced by redemption ($${Number(cDebtBefore - cDebtAfter) / 1e6})`);
  assert((await tokenBal(a.cAta)) > aCollBefore, "redeemer A received WSOL collateral");
  assert((await mintSupply()) < circBefore, "fUSD burned on redemption (circulating fell)");
  await supplyInvariant("after redemption");

  // ── Stage 8: warp 1 year -> refresh_market mints accrued interest to the buffer ──
  console.log("\n── Stage 8: warp 1y -> refresh_market (interest -> buffer) ──");
  const bufBefore = await tokenBal(bufferFusdVault);
  await warpTo((await getClock(conn)) + 365n * 24n * 3600n);
  await trySend("refresh_market", () =>
    program.methods.refreshMarket().accounts({
      collateralMint: WSOL, market, fusdMint, mintAuthority, insuranceBuffer: buffer, bufferFusdVault,
      crankerFusdAta: null, backstop: null, backstopFusdVault: null, tokenProgram: TOKEN,
    }).rpc());
  const bufAfter = await tokenBal(bufferFusdVault);
  assert(bufAfter > bufBefore, `insurance buffer received accrued interest ($${Number(bufAfter - bufBefore) / 1e6})`);
  m = await readMarket();
  console.log(`  unminted_interest now ${m.unmintedInterest.toString()} (drained to buffer)`);
  await supplyInvariant("after refresh");

  console.log("\n🎉 Full solvency + peg lifecycle verified on a mainnet fork: borrow → RP → liquidate (waterfall) → redeem (bucket) → refresh (interest→buffer), supply identity intact throughout.");
}

main().then(() => console.log("\ndone.")).catch((e) => { console.error("\nERROR:", e.message || e); process.exit(1); });
