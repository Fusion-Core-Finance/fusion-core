// Pure unit tests for the SDK's two correctness-bearing surfaces: the BigInt health math (ported
// from cdp.rs / interest.rs) and the PDA derivers (seeds mirrored from constants.rs). No chain / no
// validator — these are deterministic and run via `npm run test:sdk` (root ts-mocha).
//
// The deriver block recomputes each PDA from RAW seed-string literals and asserts it equals the
// deriver, so a drift in either SEEDS or a deriver (vs the on-chain constants.rs strings pinned here)
// fails loudly. The math block pins the exact vectors the Rust side tests (cdp.rs / interest.rs).
import { expect } from "chai";
import { PublicKey } from "@solana/web3.js";
import * as sdk from "../src/index";

const PID = sdk.FUSD_CORE_PROGRAM_ID;
const MINT = new PublicKey("So11111111111111111111111111111111111111112"); // WSOL
const OWNER = new PublicKey("11111111111111111111111111111111"); // System program (any pubkey)
const at = (seeds: (Buffer | Uint8Array)[]) =>
  PublicKey.findProgramAddressSync(seeds, PID)[0].toBase58();
const u64le = (n: bigint) => {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(n);
  return b;
};
const s = (x: string) => Buffer.from(x);

describe("SDK PDA derivers (seeds pinned to constants.rs)", () => {
  it("protocol-wide PDAs derive from the documented seeds", () => {
    expect(sdk.deriveConfig().toBase58()).to.equal(at([s("config")]));
    expect(sdk.deriveFusdMint().toBase58()).to.equal(at([s("fusd_mint")]));
    expect(sdk.deriveMintAuthority().toBase58()).to.equal(at([s("mint_authority")]));
    expect(sdk.deriveGovGate().toBase58()).to.equal(at([s("gov_gate")]));
    expect(sdk.deriveBackstop().toBase58()).to.equal(at([s("backstop")]));
    expect(sdk.deriveBackstopFusdVault().toBase58()).to.equal(at([s("backstop_fusd")]));
    expect(sdk.deriveTimelock(5n).toBase58()).to.equal(at([s("timelock"), u64le(5n)]));
    expect(sdk.deriveGlobalTimelock(0n).toBase58()).to.equal(at([s("gtimelock"), u64le(0n)]));
  });

  it("per-market PDAs derive from seed + collateral mint", () => {
    const m = MINT.toBuffer();
    expect(sdk.deriveMarket(MINT).toBase58()).to.equal(at([s("market"), m]));
    expect(sdk.deriveCollateralVault(MINT).toBase58()).to.equal(at([s("coll_vault"), m]));
    expect(sdk.deriveMarketOracle(MINT).toBase58()).to.equal(at([s("oracle"), m]));
    expect(sdk.deriveDexTwap(MINT).toBase58()).to.equal(at([s("twap"), m]));
    expect(sdk.deriveRedemptionBitmap(MINT).toBase58()).to.equal(at([s("redeem_bitmap"), m]));
    expect(sdk.deriveRateLimiter(MINT).toBase58()).to.equal(at([s("ratelimit"), m]));
    expect(sdk.deriveReactorPool(MINT).toBase58()).to.equal(at([s("reactor"), m]));
    expect(sdk.deriveEpochToScaleToSum(MINT).toBase58()).to.equal(at([s("ess"), m]));
    expect(sdk.deriveReactorFusdVault(MINT).toBase58()).to.equal(at([s("reactor_fusd"), m]));
    expect(sdk.deriveReactorCollVault(MINT).toBase58()).to.equal(at([s("reactor_coll"), m]));
    expect(sdk.deriveInsuranceBuffer(MINT).toBase58()).to.equal(at([s("buffer"), m]));
    expect(sdk.deriveBufferFusdVault(MINT).toBase58()).to.equal(at([s("buffer_fusd"), m]));
  });

  it("per-user PDAs derive from seed + mint + owner", () => {
    expect(sdk.derivePosition(MINT, OWNER).toBase58()).to.equal(
      at([s("position"), MINT.toBuffer(), OWNER.toBuffer()])
    );
    expect(sdk.deriveReactorDeposit(MINT, OWNER).toBase58()).to.equal(
      at([s("reactor_dep"), MINT.toBuffer(), OWNER.toBuffer()])
    );
  });

  it("deriveAta uses the legacy SPL Token + ATA programs", () => {
    expect(sdk.deriveAta(MINT, OWNER).toBase58()).to.equal(
      PublicKey.findProgramAddressSync(
        [OWNER.toBuffer(), sdk.TOKEN_PROGRAM_ID.toBuffer(), MINT.toBuffer()],
        sdk.ASSOCIATED_TOKEN_PROGRAM_ID
      )[0].toBase58()
    );
  });
});

describe("SDK health math (mirrors cdp.rs / interest.rs vectors)", () => {
  const SPOT = 150n * sdk.RAY; // $150 per native collateral unit, RAY-scaled

  it("collateralValue / maxDebt floor against the protocol", () => {
    expect(sdk.collateralValue(2n, SPOT)).to.equal(300n);
    expect(sdk.maxDebt(300n, 12000n)).to.equal(250n); // 120% MCR
    expect(sdk.maxDebt(300n, 10000n)).to.equal(300n); // 100% MCR
    expect(sdk.maxDebt(300n, 0n)).to.equal(0n); // div-by-zero guard
  });

  it("isHealthy boundary matches cdp.rs (<=, floored)", () => {
    expect(sdk.isHealthy(2n, 250n, SPOT, 12000n)).to.equal(true);
    expect(sdk.isHealthy(2n, 251n, SPOT, 12000n)).to.equal(false);
    expect(sdk.isHealthy(2n, 0n, SPOT, 12000n)).to.equal(true); // no debt is healthy
    expect(sdk.isHealthy(0n, 1n, SPOT, 12000n)).to.equal(false); // no collateral, has debt
    expect(sdk.isHealthy(2n, 1n, 0n, 12000n)).to.equal(false); // zero price values collateral at 0
  });

  it("collateralRatioBps is null at zero debt, else value/debt in bps", () => {
    expect(sdk.collateralRatioBps(2n, 0n, SPOT)).to.equal(null);
    expect(sdk.collateralRatioBps(2n, 300n, SPOT)).to.equal(10000n); // 300 value / 300 debt = 100%
    expect(sdk.collateralRatioBps(2n, 150n, SPOT)).to.equal(20000n); // 300 / 150 = 200%
  });

  it("maxBorrow is remaining capacity, floored at 0", () => {
    expect(sdk.maxBorrow(2n, 100n, SPOT, 12000n)).to.equal(150n); // 250 cap - 100 debt
    expect(sdk.maxBorrow(2n, 250n, SPOT, 12000n)).to.equal(0n); // at the cap
    expect(sdk.maxBorrow(2n, 300n, SPOT, 12000n)).to.equal(0n); // over the cap, no underflow
  });

  it("maxBorrow nets out the C7 upfront borrow fee (post-fee debt must stay <= maxDebt)", () => {
    expect(sdk.maxBorrow(2n, 100n, SPOT, 12000n, 0n)).to.equal(150n); // fee 0 == the fee-free answer
    // headroom 150, fee 1% ⇒ floor(150*10000/10100) = 148: borrowing 148 costs ceil(148*0.01)=2 ⇒
    // post-fee 150 == headroom (borrowing 149 would cost 2 ⇒ 151 > 150 and revert on-chain).
    expect(sdk.maxBorrow(2n, 100n, SPOT, 12000n, 100n)).to.equal(148n);
    // At MAX_BORROW_FEE_BPS (500 = 5%): floor(150*10000/10500) = 142.
    expect(sdk.maxBorrow(2n, 100n, SPOT, 12000n, 500n)).to.equal(142n);
    expect(sdk.maxBorrow(2n, 250n, SPOT, 12000n, 100n)).to.equal(0n); // no headroom, fee irrelevant
  });

  it("accruedInterest is linear and floored (the per-position direction)", () => {
    const Y = sdk.SECONDS_PER_YEAR;
    expect(sdk.accruedInterest(1_000_000_000n, 500n, Y)).to.equal(50_000_000n); // 5% of 1e9 over 1y
    expect(sdk.accruedInterest(1_000_000_000n, 500n, Y / 2n)).to.equal(25_000_000n); // half a year
    expect(sdk.accruedInterest(1_000_000_000n, 2550n, Y)).to.equal(255_000_000n); // 25.5% (max rate)
    expect(sdk.accruedInterest(1n, 50n, 1n)).to.equal(0n); // floors to 0
  });

  it("currentDebt accrues since lastDebtUpdate and clamps a backwards clock to 0", () => {
    expect(sdk.currentDebt(1_000_000_000n, 500n, 0n, sdk.SECONDS_PER_YEAR)).to.equal(1_050_000_000n);
    expect(sdk.currentDebt(1000n, 500n, 100n, 50n)).to.equal(1000n); // nowSecs < lastDebtUpdate => dt 0
  });
});

describe("positionHealth: healthy (LOW spot) vs liquidatable (HIGH debt_spot)", () => {
  // spot $150, debt_spot $160 (debt_spot >= spot, the asymmetric pair). ink 2 => value 300 @ spot,
  // 320 @ debt_spot; mcr 120% => maxDebt 250 @ spot, 266 @ debt_spot. No interest (rate 0, dt 0).
  const SPOT = 150n * sdk.RAY;
  const DEBT_SPOT = 160n * sdk.RAY;
  const market = (spot: bigint, debtSpot: bigint) => ({ spot, debtSpot, mcrBps: 12000n });
  const pos = (ink: bigint, debt: bigint) => ({
    ink,
    recordedDebt: debt,
    userRateBps: 0n,
    lastDebtUpdate: 0n,
  });

  it("the gap band: below MCR at spot but not yet liquidatable at debt_spot", () => {
    const h = sdk.positionHealth(pos(2n, 260n), market(SPOT, DEBT_SPOT), 0n);
    expect(h.healthy).to.equal(false); // 260 > maxDebt@spot 250
    expect(h.liquidatable).to.equal(false); // 260 <= maxDebt@debt_spot 266
  });

  it("clearly safe: healthy and not liquidatable", () => {
    const h = sdk.positionHealth(pos(2n, 250n), market(SPOT, DEBT_SPOT), 0n);
    expect(h.healthy).to.equal(true);
    expect(h.liquidatable).to.equal(false);
  });

  it("underwater even at debt_spot: liquidatable", () => {
    const h = sdk.positionHealth(pos(2n, 270n), market(SPOT, DEBT_SPOT), 0n);
    expect(h.healthy).to.equal(false);
    expect(h.liquidatable).to.equal(true); // 270 > maxDebt@debt_spot 266
  });

  it("unpriced market (debtSpot == 0) is fail-closed un-liquidatable", () => {
    const h = sdk.positionHealth(pos(2n, 10_000n), market(SPOT, 0n), 0n);
    expect(h.liquidatable).to.equal(false);
  });

  it("maxBorrow in the health view nets out the market's C7 borrow fee", () => {
    const noFee = sdk.positionHealth(pos(2n, 100n), market(SPOT, DEBT_SPOT), 0n);
    expect(noFee.maxBorrow).to.equal(150n); // no borrowFeeBps ⇒ raw headroom
    const withFee = sdk.positionHealth(pos(2n, 100n), { ...market(SPOT, DEBT_SPOT), borrowFeeBps: 100n }, 0n);
    expect(withFee.maxBorrow).to.equal(148n); // 1% fee applied
  });
});
