// Unit checks for the fuSOL deployment verifier (scripts/verify-fusol-deployment.ts) — the
// FAIL-CLOSED launch gate (FUSOL-08). The pure check functions take (info bytes + expected) and
// return string|null, so every one is exercised against hand-built byte fixtures with NO live RPC.
//
// The FAILURE-PATH coverage is the whole point of a security gate: for EVERY check a GOOD fixture
// passes AND each individual corruption (wrong owner, wrong authority, freeze-set, wrong fee,
// non-None upgrade authority, unsealed config, wrong hash, missing account, …) FAILS. A gate that
// silently passes a bad deployment is worse than none.
//
// Run via `yarn test:sdk` (root ts-mocha globs keepers/**/*.spec.ts).
import assert from "node:assert";
import { createHash } from "node:crypto";
import { PublicKey } from "@solana/web3.js";
import {
  Acct,
  anchorDiscriminator,
  parseProgramDataAddress,
  parseUpgradeAuthority,
  checkProgramImmutability,
  checkProgramElfHash,
  extractSecurityTxt,
  checkFusolMint,
  readMintSupply,
  checkMintSupplyMatchesPool,
  checkMintFreezeNone,
  parseStakePool,
  checkStakePool,
  calculateMaxValidators,
  checkValidatorList,
  checkMaintenanceVault,
  checkReserveStake,
  checkControllerConfig,
  checkMarketOracle,
  checkMarket,
  validateConfig,
  derivedPdaErrors,
  TOKEN_PROGRAM,
  BPF_LOADER_UPGRADEABLE,
  STAKE_PROGRAM,
  ZERO_KEY,
  PYTH_SOL_USD_FEED_ID,
  MAX_VALIDATORS,
  VALIDATOR_LIST_SPACE,
  LIQ_INFRA_REACTOR_POOL,
  LIQ_INFRA_INSURANCE_BUFFER,
  LIQ_INFRA_READY_MASK,
  MARKET_ORACLE_SPACE,
  MARKET_SPACE,
  CONTROLLER_CONFIG_SPACE,
  STAKE_POOL_SPACE,
} from "../scripts/verify-fusol-deployment";
import {
  CONTROLLER_PROGRAM_ID,
  STAKE_POOL_FORK_ID,
  controllerConfig as deriveControllerConfig,
  poolAuthority as derivePoolAuthority,
  depositAuthority as deriveDepositAuthority,
  maintenanceAuthority as deriveMaintenanceAuthority,
  poolWithdrawAuthority as derivePoolWithdrawAuthority,
} from "../sdk/src/stake-pool";

// ── fixed test pubkeys (internal-consistency only; the pure fns take expected values as params) ───
const KEY = (n: number) => new PublicKey(new Uint8Array(32).fill(n));
const MANAGER = KEY(11);
const DEPOSIT = KEY(12);
const VLIST = KEY(13);
const RESERVE = KEY(14);
const MINT = KEY(15);
const VAULT = KEY(16);
const MINT_AUTH = KEY(17);
const VAULT_AUTH = KEY(18);
const STAKE_POOL = KEY(19);
const FUSDCORE = KEY(20);
const OTHER = KEY(99);

// ── a mirror-image byte writer for building fixtures ─────────────────────────────────────────────
class W {
  buf: Buffer;
  off = 0;
  constructor(size: number) {
    this.buf = Buffer.alloc(size);
  }
  u8(v: number): this {
    this.buf.writeUInt8(v, this.off);
    this.off += 1;
    return this;
  }
  u16(v: number): this {
    this.buf.writeUInt16LE(v, this.off);
    this.off += 2;
    return this;
  }
  u32(v: number): this {
    this.buf.writeUInt32LE(v, this.off);
    this.off += 4;
    return this;
  }
  u64(v: bigint): this {
    this.buf.writeBigUInt64LE(v, this.off);
    this.off += 8;
    return this;
  }
  pk(k: PublicKey): this {
    k.toBuffer().copy(this.buf, this.off);
    this.off += 32;
    return this;
  }
  bytes(b: Buffer): this {
    b.copy(this.buf, this.off);
    this.off += b.length;
    return this;
  }
  skip(n: number): this {
    this.off += n;
    return this;
  }
  fee(num: bigint, denom: bigint): this {
    return this.u64(denom).u64(num); // vendor Fee { denominator FIRST, numerator }
  }
  futureNone(): this {
    return this.u8(0);
  }
  optNone(): this {
    return this.u8(0);
  }
  optSome(k: PublicKey): this {
    return this.u8(1).pk(k);
  }
}
const acct = (owner: PublicKey, data: Buffer, executable = false): Acct => ({
  owner,
  data,
  executable,
});
// Reshape a canonical-size fixture to a wrong `size` (for the size-guard corruption tests) without
// disturbing the fixed-offset pokes, which are written into the full-size buffer first.
const resize = (buf: Buffer, size?: number): Buffer => {
  if (size === undefined || size === buf.length) return buf;
  const out = Buffer.alloc(size);
  buf.copy(out, 0, 0, Math.min(size, buf.length));
  return out;
};

describe("verify-fusol-deployment / check 1 — programs executable + upgrade authority", () => {
  const PROGRAMDATA = KEY(50);
  const AUTH = KEY(51);
  const programAcct = (
    owner = BPF_LOADER_UPGRADEABLE,
    pd = PROGRAMDATA,
    executable = true,
    tag = 2,
  ): Acct => {
    const w = new W(36).u32(tag).pk(pd);
    return acct(owner, w.buf, executable);
  };
  const programDataAcct = (
    authority: PublicKey | null,
    owner = BPF_LOADER_UPGRADEABLE,
    tag = 3,
  ): Acct => {
    // tag u32 + slot u64 + Option<Pubkey> + a little ELF payload
    const w = new W(45 + 8);
    w.u32(tag).u64(123n);
    if (authority) w.u8(1).pk(authority);
    else w.u8(0).skip(32);
    w.off = 45;
    w.bytes(Buffer.from("ELF!ELF!")); // 8 bytes of "ELF"
    return acct(owner, w.buf);
  };

  it("good: an authority-guarded program (pre-seal) passes", () => {
    assert.equal(
      checkProgramImmutability(
        programAcct(),
        programDataAcct(AUTH),
        PROGRAMDATA,
        AUTH,
      ),
      null,
    );
  });
  it('good: a renounced program (authority None, expect "none") passes', () => {
    assert.equal(
      checkProgramImmutability(
        programAcct(),
        programDataAcct(null),
        PROGRAMDATA,
        "none",
      ),
      null,
    );
  });
  it('FAIL: non-None upgrade authority when expect "none" (the headline immutability failure)', () => {
    const e = checkProgramImmutability(
      programAcct(),
      programDataAcct(AUTH),
      PROGRAMDATA,
      "none",
    );
    assert.match(e!, /NOT renounced/);
  });
  it("FAIL: upgrade authority mismatch when a specific key is expected", () => {
    assert.match(
      checkProgramImmutability(
        programAcct(),
        programDataAcct(OTHER),
        PROGRAMDATA,
        AUTH,
      )!,
      /upgrade authority is/,
    );
  });
  it("FAIL: upgrade authority None when a specific key is expected", () => {
    assert.match(
      checkProgramImmutability(
        programAcct(),
        programDataAcct(null),
        PROGRAMDATA,
        AUTH,
      )!,
      /None \(expected/,
    );
  });
  it("FAIL: program not executable", () => {
    assert.match(
      checkProgramImmutability(
        programAcct(BPF_LOADER_UPGRADEABLE, PROGRAMDATA, false),
        programDataAcct(AUTH),
        PROGRAMDATA,
        AUTH,
      )!,
      /not executable/,
    );
  });
  it("FAIL: program owner is not the upgradeable loader", () => {
    assert.match(
      checkProgramImmutability(
        programAcct(OTHER),
        programDataAcct(AUTH),
        PROGRAMDATA,
        AUTH,
      )!,
      /not the BPF upgradeable loader/,
    );
  });
  it("FAIL: embedded programdata address != derived ProgramData PDA (spoof)", () => {
    assert.match(
      checkProgramImmutability(
        programAcct(BPF_LOADER_UPGRADEABLE, OTHER),
        programDataAcct(AUTH),
        PROGRAMDATA,
        AUTH,
      )!,
      /!= derived ProgramData PDA/,
    );
  });
  it("FAIL: programdata owner is not the loader", () => {
    assert.match(
      checkProgramImmutability(
        programAcct(),
        programDataAcct(AUTH, OTHER),
        PROGRAMDATA,
        AUTH,
      )!,
      /programdata owner .* is not the BPF/,
    );
  });
  it("FAIL: missing program account, missing programdata account", () => {
    assert.match(
      checkProgramImmutability(null, programDataAcct(AUTH), PROGRAMDATA, AUTH)!,
      /program account is missing/,
    );
    assert.match(
      checkProgramImmutability(programAcct(), null, PROGRAMDATA, AUTH)!,
      /programdata account is missing/,
    );
  });
  it("FAIL: wrong state tags surface as parse errors", () => {
    assert.match(
      checkProgramImmutability(
        programAcct(BPF_LOADER_UPGRADEABLE, PROGRAMDATA, true, 3),
        programDataAcct(AUTH),
        PROGRAMDATA,
        AUTH,
      )!,
      /parse failed/,
    );
    assert.match(
      checkProgramImmutability(
        programAcct(),
        programDataAcct(AUTH, BPF_LOADER_UPGRADEABLE, 2),
        PROGRAMDATA,
        AUTH,
      )!,
      /parse failed/,
    );
  });

  it("parseProgramDataAddress + parseUpgradeAuthority: tags, options, truncation", () => {
    assert.ok(parseProgramDataAddress(programAcct().data).equals(PROGRAMDATA));
    assert.throws(
      () => parseProgramDataAddress(new W(36).u32(3).pk(PROGRAMDATA).buf),
      /expected 2/,
    );
    assert.throws(() => parseProgramDataAddress(Buffer.alloc(10)), /too short/);
    const ua = parseUpgradeAuthority(programDataAcct(AUTH).data);
    assert.equal(ua.slot, 123n);
    assert.ok(ua.authority!.equals(AUTH));
    assert.equal(
      parseUpgradeAuthority(programDataAcct(null).data).authority,
      null,
    );
    assert.throws(
      () => parseUpgradeAuthority(new W(13).u32(3).u64(1n).u8(2).buf),
      /invalid upgrade-authority Option tag/,
    );
    assert.throws(
      () => parseUpgradeAuthority(new W(45).u32(5).buf),
      /expected 3/,
    );
    assert.throws(() => parseUpgradeAuthority(Buffer.alloc(5)), /too short/);
  });
});

describe("verify-fusol-deployment / check 2 — program ELF hash + security.txt", () => {
  const withElf = (elf: Buffer): Acct => {
    const w = new W(45 + elf.length).u32(3).u64(1n).u8(0);
    w.off = 45;
    w.bytes(elf);
    return acct(BPF_LOADER_UPGRADEABLE, w.buf);
  };
  const elf = Buffer.from("the-program-bytes-vXYZ");
  const hash = createHash("sha256").update(elf).digest("hex");

  it("good: matching pinned hash passes", () => {
    const r = checkProgramElfHash(withElf(elf), hash);
    assert.equal(r.status, "PASS");
    assert.equal(r.actualHex, hash);
  });
  it("good: 0x-prefixed + uppercase hash still matches", () => {
    assert.equal(
      checkProgramElfHash(withElf(elf), "0x" + hash.toUpperCase()).status,
      "PASS",
    );
  });
  it("FAIL: wrong hash", () => {
    const r = checkProgramElfHash(withElf(elf), "a".repeat(64));
    assert.equal(r.status, "FAIL");
    assert.match(r.detail, /!= expected/);
  });
  it("NOT_VERIFIED (a non-pass): no expected hash pinned in config", () => {
    const r = checkProgramElfHash(withElf(elf), undefined);
    assert.equal(r.status, "NOT_VERIFIED");
    assert.notEqual(r.status, "PASS");
  });
  it("FAIL: missing programdata account / too short to hold an ELF", () => {
    assert.equal(checkProgramElfHash(null, hash).status, "FAIL");
    assert.equal(
      checkProgramElfHash(acct(BPF_LOADER_UPGRADEABLE, Buffer.alloc(45)), hash)
        .status,
      "FAIL",
    );
  });
  it("extractSecurityTxt parses embedded pairs; returns null when absent", () => {
    const body = ["name", "fuSOL", "contacts", "security@example.test"].join(
      "\0",
    );
    const blob = Buffer.concat([
      Buffer.from("prefix junk"),
      Buffer.from("=======BEGIN SECURITY.TXT V1=======\0"),
      Buffer.from(body),
      Buffer.from("=======END SECURITY.TXT V1=======\0"),
      Buffer.from("suffix"),
    ]);
    const st = extractSecurityTxt(blob)!;
    assert.equal(st.name, "fuSOL");
    assert.equal(st.contacts, "security@example.test");
    assert.equal(extractSecurityTxt(Buffer.from("no security txt here")), null);
  });
});

describe("verify-fusol-deployment / check 3 — fuSOL mint", () => {
  const mint = (
    o: {
      owner?: PublicKey;
      size?: number;
      mintAuthTag?: number;
      mintAuth?: PublicKey;
      supply?: bigint;
      decimals?: number;
      init?: number;
      freezeTag?: number;
      freeze?: PublicKey;
    } = {},
  ): Acct => {
    const w = new W(o.size ?? 82);
    w.u32(o.mintAuthTag ?? 1).pk(o.mintAuth ?? MINT_AUTH); // mint_authority COption @0
    w.u64(o.supply ?? 1_000_000_000n); // supply @36
    w.u8(o.decimals ?? 9); // decimals @44
    w.u8(o.init ?? 1); // is_initialized @45
    w.u32(o.freezeTag ?? 0); // freeze_authority COption tag @46
    if (o.freeze) o.freeze.toBuffer().copy(w.buf, 50);
    return acct(o.owner ?? TOKEN_PROGRAM, w.buf);
  };

  it("good passes", () =>
    assert.equal(checkFusolMint(mint(), MINT_AUTH), null));
  it("FAIL: wrong owner", () =>
    assert.match(
      checkFusolMint(mint({ owner: OTHER }), MINT_AUTH)!,
      /not the SPL Token program/,
    ));
  it("FAIL: wrong size", () =>
    assert.match(
      checkFusolMint(mint({ size: 165 }), MINT_AUTH)!,
      /size 165 != 82/,
    ));
  it("FAIL: not initialized", () =>
    assert.match(
      checkFusolMint(mint({ init: 0 }), MINT_AUTH)!,
      /not initialized/,
    ));
  it("FAIL: wrong decimals", () =>
    assert.match(
      checkFusolMint(mint({ decimals: 6 }), MINT_AUTH)!,
      /decimals 6 != 9/,
    ));
  it("FAIL: wrong mint authority", () =>
    assert.match(
      checkFusolMint(mint({ mintAuth: OTHER }), MINT_AUTH)!,
      /mint authority is not/,
    ));
  it("FAIL: mint authority is None", () =>
    assert.match(
      checkFusolMint(mint({ mintAuthTag: 0 }), MINT_AUTH)!,
      /mint authority is not/,
    ));
  it("FAIL: FREEZE AUTHORITY SET (the audit-named live check)", () =>
    assert.match(
      checkFusolMint(mint({ freezeTag: 1, freeze: OTHER }), MINT_AUTH)!,
      /freeze authority is set/,
    ));
  it("FAIL: supply 0", () =>
    assert.match(
      checkFusolMint(mint({ supply: 0n }), MINT_AUTH)!,
      /supply is 0/,
    ));
  it("FAIL: missing account", () =>
    assert.match(checkFusolMint(null, MINT_AUTH)!, /missing/));

  it("supply == pool_token_supply cross-invariant", () => {
    assert.equal(readMintSupply(mint({ supply: 42n }).data), 42n);
    assert.equal(
      checkMintSupplyMatchesPool(mint({ supply: 42n }).data, 42n),
      null,
    );
    assert.match(
      checkMintSupplyMatchesPool(mint({ supply: 42n }).data, 43n)!,
      /!= pool_token_supply/,
    );
  });
  it("checkMintFreezeNone (check-10 observable): none passes, set fails", () => {
    assert.equal(checkMintFreezeNone(mint()), null);
    assert.match(
      checkMintFreezeNone(mint({ freezeTag: 1, freeze: OTHER }))!,
      /freeze authority is set/,
    );
    assert.match(checkMintFreezeNone(null)!, /missing/);
  });
});

describe("verify-fusol-deployment / check 4 — StakePool authority graph + fee schedule", () => {
  interface SP {
    accountType?: number;
    manager?: PublicKey;
    staker?: PublicKey;
    stakeDepositAuthority?: PublicKey;
    validatorList?: PublicKey;
    reserveStake?: PublicKey;
    poolMint?: PublicKey;
    managerFeeAccount?: PublicKey;
    tokenProgram?: PublicKey;
    totalLamports?: bigint;
    poolTokenSupply?: bigint;
    epochFee?: [bigint, bigint];
    nextEpochFee?: PublicKey | "set";
    stakeDepositFee?: [bigint, bigint];
    stakeWithdrawalFee?: [bigint, bigint];
    stakeReferralFee?: number;
    solDepositAuthority?: PublicKey | null;
    solDepositFee?: [bigint, bigint];
    solReferralFee?: number;
    solWithdrawAuthority?: PublicKey | null;
    solWithdrawalFee?: [bigint, bigint];
    size?: number;
  }
  const build = (o: SP = {}): Acct => {
    const w = new W(o.size ?? STAKE_POOL_SPACE);
    w.u8(o.accountType ?? 1);
    w.pk(o.manager ?? MANAGER);
    w.pk(o.staker ?? MANAGER);
    w.pk(o.stakeDepositAuthority ?? DEPOSIT);
    w.u8(254); // stake_withdraw_bump_seed
    w.pk(o.validatorList ?? VLIST);
    w.pk(o.reserveStake ?? RESERVE);
    w.pk(o.poolMint ?? MINT);
    w.pk(o.managerFeeAccount ?? VAULT);
    w.pk(o.tokenProgram ?? TOKEN_PROGRAM);
    w.u64(o.totalLamports ?? 1_000_000_000n);
    w.u64(o.poolTokenSupply ?? 1_000_000_000n);
    w.u64(0n); // last_update_epoch
    w.skip(48); // lockup (zero)
    w.fee(...(o.epochFee ?? [1n, 100n]));
    if (o.nextEpochFee === "set") w.u8(1).fee(1n, 100n);
    else w.futureNone(); // next_epoch_fee
    w.optNone(); // preferred_deposit
    w.optNone(); // preferred_withdraw
    w.fee(...(o.stakeDepositFee ?? [5n, 10_000n]));
    w.fee(...(o.stakeWithdrawalFee ?? [5n, 10_000n]));
    w.futureNone(); // next_stake_withdrawal_fee
    w.u8(o.stakeReferralFee ?? 0);
    const sda =
      o.solDepositAuthority === undefined ? DEPOSIT : o.solDepositAuthority;
    sda ? w.optSome(sda) : w.optNone();
    w.fee(...(o.solDepositFee ?? [5n, 10_000n]));
    w.u8(o.solReferralFee ?? 0);
    const swa =
      o.solWithdrawAuthority === undefined ? null : o.solWithdrawAuthority;
    swa ? w.optSome(swa) : w.optNone();
    w.fee(...(o.solWithdrawalFee ?? [5n, 10_000n]));
    w.futureNone(); // next_sol_withdrawal_fee
    w.u64(0n); // last_epoch_pool_token_supply
    w.u64(0n); // last_epoch_total_lamports
    return acct(STAKE_POOL_FORK_ID, w.buf); // owner overridden inline in the wrong-owner test
  };
  const exp = {
    poolAuthority: MANAGER,
    depositAuthority: DEPOSIT,
    validatorList: VLIST,
    reserveStake: RESERVE,
    fusolMint: MINT,
    maintenanceVault: VAULT,
  };

  it("good passes and round-trips through parseStakePool", () => {
    assert.equal(checkStakePool(build(), exp), null);
    const p = parseStakePool(build().data);
    assert.ok(p.manager.equals(MANAGER));
    assert.ok(p.solDepositAuthority!.equals(DEPOSIT));
    assert.equal(p.solWithdrawAuthority, null);
    assert.deepEqual(p.epochFee, { denominator: 100n, numerator: 1n });
  });
  it("FAIL: wrong owner", () =>
    assert.match(
      checkStakePool(acct(OTHER, build().data), exp)!,
      /not the stake-pool fork/,
    ));
  it("FAIL: wrong size", () =>
    assert.match(
      checkStakePool(build({ size: 610 }), exp)!,
      /size 610 != 611/,
    ));
  it("FAIL: account_type != StakePool", () =>
    assert.match(
      checkStakePool(build({ accountType: 2 }), exp)!,
      /account_type 2 != StakePool/,
    ));
  it("FAIL: manager / staker / deposit authority mismatches (wrong authority)", () => {
    assert.match(checkStakePool(build({ manager: OTHER }), exp)!, /manager is/);
    assert.match(checkStakePool(build({ staker: OTHER }), exp)!, /staker is/);
    assert.match(
      checkStakePool(build({ stakeDepositAuthority: OTHER }), exp)!,
      /stake_deposit_authority is/,
    );
  });
  it("FAIL: validator_list / reserve / mint / fee vault / token program mismatches", () => {
    assert.match(
      checkStakePool(build({ validatorList: OTHER }), exp)!,
      /validator_list is/,
    );
    assert.match(
      checkStakePool(build({ reserveStake: OTHER }), exp)!,
      /reserve_stake is/,
    );
    assert.match(
      checkStakePool(build({ poolMint: OTHER }), exp)!,
      /pool_mint is/,
    );
    assert.match(
      checkStakePool(build({ managerFeeAccount: OTHER }), exp)!,
      /manager_fee_account is/,
    );
    assert.match(
      checkStakePool(build({ tokenProgram: OTHER }), exp)!,
      /token_program_id is/,
    );
  });
  it("FAIL: wrong fees (epoch, deposit, withdrawal)", () => {
    assert.match(
      checkStakePool(build({ epochFee: [1n, 50n] }), exp)!,
      /epoch_fee is 1\/50/,
    );
    assert.match(
      checkStakePool(build({ stakeDepositFee: [10n, 10_000n] }), exp)!,
      /stake_deposit_fee is/,
    );
    assert.match(
      checkStakePool(build({ solDepositFee: [0n, 0n] }), exp)!,
      /sol_deposit_fee is/,
    );
    assert.match(
      checkStakePool(build({ stakeWithdrawalFee: [6n, 10_000n] }), exp)!,
      /stake_withdrawal_fee is/,
    );
    assert.match(
      checkStakePool(build({ solWithdrawalFee: [1n, 100n] }), exp)!,
      /sol_withdrawal_fee is/,
    );
  });
  it("FAIL: nonzero referral fees", () => {
    assert.match(
      checkStakePool(build({ stakeReferralFee: 1 }), exp)!,
      /stake_referral_fee is 1/,
    );
    assert.match(
      checkStakePool(build({ solReferralFee: 5 }), exp)!,
      /sol_referral_fee is 5/,
    );
  });
  it("FAIL: a scheduled (next_epoch) fee change is pending", () => {
    assert.match(
      checkStakePool(build({ nextEpochFee: "set" }), exp)!,
      /next_epoch_fee is set/,
    );
  });
  it("FAIL: sol_deposit_authority None or wrong (deposits must route through the controller)", () => {
    assert.match(
      checkStakePool(build({ solDepositAuthority: null }), exp)!,
      /sol_deposit_authority is None/,
    );
    assert.match(
      checkStakePool(build({ solDepositAuthority: OTHER }), exp)!,
      /sol_deposit_authority is/,
    );
  });
  it("FAIL: sol_withdraw_authority SET (the SOL exit must never be gated)", () => {
    assert.match(
      checkStakePool(build({ solWithdrawAuthority: OTHER }), exp)!,
      /sol_withdraw_authority is .*must be None/,
    );
  });
  it("FAIL: zero total_lamports / pool_token_supply", () => {
    assert.match(
      checkStakePool(build({ totalLamports: 0n }), exp)!,
      /total_lamports is 0/,
    );
    assert.match(
      checkStakePool(build({ poolTokenSupply: 0n }), exp)!,
      /pool_token_supply is 0/,
    );
  });
  it("FAIL-closed: missing account, wrong-size guard, and a truncated parse throws", () => {
    assert.match(checkStakePool(null, exp)!, /missing/);
    // A wrong-size fork-owned buffer is rejected by the size guard before any parse.
    assert.match(
      checkStakePool(
        acct(STAKE_POOL_FORK_ID, build().data.subarray(0, 350)),
        exp,
      )!,
      /size 350 != 611/,
    );
    // parseStakePool itself fails closed (throws) on a buffer too short to hold the head/tail.
    assert.throws(
      () => parseStakePool(build().data.subarray(0, 300)),
      /truncated/,
    );
  });
});

describe("verify-fusol-deployment / check 5 — ValidatorList", () => {
  const list = (
    o: { owner?: PublicKey; type?: number; max?: number; size?: number } = {},
  ): Acct => {
    const size = o.size ?? VALIDATOR_LIST_SPACE;
    const w = new W(size)
      .u8(o.type ?? 2)
      .u32(o.max ?? MAX_VALIDATORS)
      .u32(0);
    return acct(o.owner ?? STAKE_POOL_FORK_ID, w.buf);
  };
  it("good passes; calculateMaxValidators matches the account size", () => {
    assert.equal(checkValidatorList(list()), null);
    assert.equal(calculateMaxValidators(VALIDATOR_LIST_SPACE), MAX_VALIDATORS);
  });
  it("FAIL: wrong owner", () =>
    assert.match(
      checkValidatorList(list({ owner: OTHER }))!,
      /not the stake-pool fork/,
    ));
  it("FAIL: account_type != ValidatorList", () =>
    assert.match(
      checkValidatorList(list({ type: 1 }))!,
      /account_type 1 != ValidatorList/,
    ));
  it("FAIL: stored max_validators != 1024", () =>
    assert.match(
      checkValidatorList(list({ max: 1000 }))!,
      /max_validators 1000 != 1024/,
    ));
  it("FAIL: account size holds a different capacity", () =>
    assert.match(
      checkValidatorList(list({ size: VALIDATOR_LIST_SPACE - 73 }))!,
      /validators, expected 1024/,
    ));
  it("FAIL: header-short buffer, missing account", () => {
    assert.match(
      checkValidatorList(acct(STAKE_POOL_FORK_ID, Buffer.alloc(8)))!,
      /shorter than the 9-byte header/,
    );
    assert.match(checkValidatorList(null)!, /missing/);
  });
});

describe("verify-fusol-deployment / check 6 — maintenance vault", () => {
  const vault = (
    o: {
      owner?: PublicKey;
      size?: number;
      state?: number;
      mint?: PublicKey;
      authority?: PublicKey;
      delegateTag?: number;
      closeTag?: number;
    } = {},
  ): Acct => {
    const w = new W(165);
    w.pk(o.mint ?? MINT); // mint @0
    w.pk(o.authority ?? VAULT_AUTH); // owner @32
    w.u64(0n); // amount @64
    w.u32(o.delegateTag ?? 0); // delegate COption @72
    w.skip(32); // delegate key
    w.buf.writeUInt8(o.state ?? 1, 108); // state @108
    w.buf.writeUInt32LE(o.closeTag ?? 0, 129); // close_authority COption @129
    return acct(o.owner ?? TOKEN_PROGRAM, resize(w.buf, o.size));
  };
  it("good passes", () =>
    assert.equal(checkMaintenanceVault(vault(), MINT, VAULT_AUTH), null));
  it("FAIL: wrong owner", () =>
    assert.match(
      checkMaintenanceVault(vault({ owner: OTHER }), MINT, VAULT_AUTH)!,
      /not the SPL Token program/,
    ));
  it("FAIL: wrong size", () =>
    assert.match(
      checkMaintenanceVault(vault({ size: 82 }), MINT, VAULT_AUTH)!,
      /size 82 != 165/,
    ));
  it("FAIL: not initialized", () =>
    assert.match(
      checkMaintenanceVault(vault({ state: 0 }), MINT, VAULT_AUTH)!,
      /not in the Initialized state/,
    ));
  it("FAIL: wrong mint", () =>
    assert.match(
      checkMaintenanceVault(vault({ mint: OTHER }), MINT, VAULT_AUTH)!,
      /mint is not the fuSOL mint/,
    ));
  it("FAIL: wrong authority", () =>
    assert.match(
      checkMaintenanceVault(vault({ authority: OTHER }), MINT, VAULT_AUTH)!,
      /token authority is not/,
    ));
  it("FAIL: delegate set", () =>
    assert.match(
      checkMaintenanceVault(vault({ delegateTag: 1 }), MINT, VAULT_AUTH)!,
      /delegate is set/,
    ));
  it("FAIL: close authority set", () =>
    assert.match(
      checkMaintenanceVault(vault({ closeTag: 1 }), MINT, VAULT_AUTH)!,
      /close authority is set/,
    ));
  it("FAIL: missing account", () =>
    assert.match(checkMaintenanceVault(null, MINT, VAULT_AUTH)!, /missing/));
});

describe("verify-fusol-deployment / check 7 — reserve stake", () => {
  const PW = KEY(70);
  const reserve = (
    o: {
      owner?: PublicKey;
      size?: number;
      tag?: number;
      staker?: PublicKey;
      withdrawer?: PublicKey;
      lockupByte?: [number, number];
    } = {},
  ): Acct => {
    const w = new W(o.size ?? 200);
    w.u32(o.tag ?? 1); // discriminant @0
    w.u64(0n); // rent_exempt_reserve @4
    w.pk(o.staker ?? PW); // authorized.staker @12
    w.pk(o.withdrawer ?? PW); // authorized.withdrawer @44
    if (o.lockupByte) w.buf.writeUInt8(o.lockupByte[1], o.lockupByte[0]); // poke a lockup byte in @76..124
    return acct(o.owner ?? STAKE_PROGRAM, w.buf);
  };
  it("good passes (Initialized and Stake states)", () => {
    assert.equal(checkReserveStake(reserve(), PW), null);
    assert.equal(checkReserveStake(reserve({ tag: 2 }), PW), null);
  });
  it("FAIL: wrong owner", () =>
    assert.match(
      checkReserveStake(reserve({ owner: OTHER }), PW)!,
      /not the stake program/,
    ));
  it("FAIL: wrong size", () =>
    assert.match(
      checkReserveStake(reserve({ size: 100 }), PW)!,
      /size 100 != 200/,
    ));
  it("FAIL: bad state discriminant", () =>
    assert.match(
      checkReserveStake(reserve({ tag: 0 }), PW)!,
      /neither Initialized/,
    ));
  it("FAIL: wrong staker / withdrawer", () => {
    assert.match(
      checkReserveStake(reserve({ staker: OTHER }), PW)!,
      /staker is not/,
    );
    assert.match(
      checkReserveStake(reserve({ withdrawer: OTHER }), PW)!,
      /withdrawer is not/,
    );
  });
  it("FAIL: a lockup is set (custodian / timestamp / epoch non-zero)", () => {
    assert.match(
      checkReserveStake(reserve({ lockupByte: [76, 1] }), PW)!,
      /lockup set/,
    ); // unix_timestamp
    assert.match(
      checkReserveStake(reserve({ lockupByte: [92, 7] }), PW)!,
      /lockup set/,
    ); // custodian
  });
  it("FAIL: missing account", () =>
    assert.match(checkReserveStake(null, PW)!, /missing/));
});

describe("verify-fusol-deployment / check 8 — ControllerConfig", () => {
  const exp = {
    stakePoolProgram: STAKE_POOL_FORK_ID,
    stakePool: STAKE_POOL,
    validatorList: VLIST,
    reserveStake: RESERVE,
    fusolMint: MINT,
    poolWithdrawAuthority: KEY(30),
    maintenanceVault: VAULT,
    fusdCoreProgram: FUSDCORE,
    bump: 250,
    poolAuthorityBump: 251,
    depositAuthorityBump: 252,
    maintenanceAuthorityBump: 253,
  };
  const cc = (
    o: Partial<{
      owner: PublicKey;
      size: number;
      disc: Buffer;
      version: number;
      sealed: number;
      stakePoolProgram: PublicKey;
      stakePool: PublicKey;
      validatorList: PublicKey;
      reserveStake: PublicKey;
      fusolMint: PublicKey;
      poolWithdrawAuthority: PublicKey;
      maintenanceVault: PublicKey;
      fusdCoreProgram: PublicKey;
      collateralMint: PublicKey;
      bump: number;
    }> = {},
  ): Acct => {
    const w = new W(CONTROLLER_CONFIG_SPACE);
    w.bytes(o.disc ?? anchorDiscriminator("ControllerConfig"));
    w.u8(o.version ?? 1);
    w.u8(o.sealed ?? 1);
    w.pk(o.stakePoolProgram ?? exp.stakePoolProgram);
    w.pk(o.stakePool ?? exp.stakePool);
    w.pk(o.validatorList ?? exp.validatorList);
    w.pk(o.reserveStake ?? exp.reserveStake);
    w.pk(o.fusolMint ?? exp.fusolMint);
    w.pk(o.poolWithdrawAuthority ?? exp.poolWithdrawAuthority);
    w.pk(o.maintenanceVault ?? exp.maintenanceVault);
    w.pk(o.fusdCoreProgram ?? exp.fusdCoreProgram);
    w.pk(o.collateralMint ?? exp.fusolMint); // fusol_collateral_mint MUST equal fusol_mint
    w.u8(o.bump ?? exp.bump);
    w.u8(exp.poolAuthorityBump);
    w.u8(exp.depositAuthorityBump);
    w.u8(exp.maintenanceAuthorityBump);
    return acct(o.owner ?? CONTROLLER_PROGRAM_ID, resize(w.buf, o.size));
  };
  it("good passes", () => assert.equal(checkControllerConfig(cc(), exp), null));
  it("FAIL: wrong owner / size / discriminator", () => {
    assert.match(
      checkControllerConfig(cc({ owner: OTHER }), exp)!,
      /not the controller program/,
    );
    assert.match(
      checkControllerConfig(cc({ size: 300 }), exp)!,
      /size 300 != 366/,
    );
    assert.match(
      checkControllerConfig(cc({ disc: Buffer.alloc(8) }), exp)!,
      /wrong anchor discriminator/,
    );
  });
  it("FAIL: wrong version", () =>
    assert.match(
      checkControllerConfig(cc({ version: 2 }), exp)!,
      /version 2 != 1/,
    ));
  it("FAIL: NOT SEALED (initialize_pool has not run)", () =>
    assert.match(checkControllerConfig(cc({ sealed: 0 }), exp)!, /not sealed/));
  it("FAIL: each recorded address mismatch", () => {
    assert.match(
      checkControllerConfig(cc({ stakePoolProgram: OTHER }), exp)!,
      /stake_pool_program is/,
    );
    assert.match(
      checkControllerConfig(cc({ stakePool: OTHER }), exp)!,
      /stake_pool is/,
    );
    assert.match(
      checkControllerConfig(cc({ validatorList: OTHER }), exp)!,
      /validator_list is/,
    );
    assert.match(
      checkControllerConfig(cc({ reserveStake: OTHER }), exp)!,
      /reserve_stake is/,
    );
    assert.match(
      checkControllerConfig(cc({ fusolMint: OTHER }), exp)!,
      /fusol_mint is/,
    );
    assert.match(
      checkControllerConfig(cc({ poolWithdrawAuthority: OTHER }), exp)!,
      /pool_withdraw_authority is/,
    );
    assert.match(
      checkControllerConfig(cc({ maintenanceVault: OTHER }), exp)!,
      /maintenance_vault is/,
    );
    assert.match(
      checkControllerConfig(cc({ fusdCoreProgram: OTHER }), exp)!,
      /fusd_core_program is/,
    );
    assert.match(
      checkControllerConfig(cc({ collateralMint: OTHER }), exp)!,
      /fusol_collateral_mint is/,
    );
  });
  it("FAIL: non-canonical bump", () =>
    assert.match(
      checkControllerConfig(cc({ bump: 1 }), exp)!,
      /bump is 1, expected the canonical bump 250/,
    ));
  it("FAIL: missing account", () =>
    assert.match(checkControllerConfig(null, exp)!, /missing/));
});

describe("verify-fusol-deployment / check 9a — fuSOL MarketOracle", () => {
  const exp = {
    fusdCoreProgram: FUSDCORE,
    fusolMint: MINT,
    stakePool: STAKE_POOL,
  };
  const oracle = (
    o: Partial<{
      owner: PublicKey;
      size: number;
      disc: Buffer;
      collateralMint: PublicKey;
      pyth: Buffer;
      orca: PublicKey;
      raydium: PublicKey;
      lst: PublicKey;
      canonical: number;
      haircut: number;
    }> = {},
  ): Acct => {
    const w = new W(MARKET_ORACLE_SPACE);
    w.bytes(o.disc ?? anchorDiscriminator("MarketOracle"));
    w.pk(o.collateralMint ?? MINT); // @8
    w.bytes(o.pyth ?? PYTH_SOL_USD_FEED_ID); // @40
    w.pk(KEY(40)); // switchboard_feed @72
    w.pk(o.orca ?? ZERO_KEY); // orca_pool @104
    w.pk(o.raydium ?? ZERO_KEY); // raydium_pool @136
    w.buf.writeUInt8(o.canonical ?? 1, 305); // canonical_primary @305
    w.buf.writeUInt16LE(o.haircut ?? 500, 306); // liquidity_haircut_bps @306
    (o.lst ?? STAKE_POOL).toBuffer().copy(w.buf, 272); // lst_stake_pool @272
    return acct(o.owner ?? FUSDCORE, resize(w.buf, o.size));
  };
  it("good passes", () => assert.equal(checkMarketOracle(oracle(), exp), null));
  it("FAIL: wrong owner / size / discriminator", () => {
    assert.match(
      checkMarketOracle(oracle({ owner: OTHER }), exp)!,
      /not the fusd-core program/,
    );
    assert.match(
      checkMarketOracle(oracle({ size: 300 }), exp)!,
      /size 300 != 335/,
    );
    assert.match(
      checkMarketOracle(oracle({ disc: Buffer.alloc(8) }), exp)!,
      /wrong anchor discriminator/,
    );
  });
  it("FAIL: collateral_mint not fuSOL", () =>
    assert.match(
      checkMarketOracle(oracle({ collateralMint: OTHER }), exp)!,
      /collateral_mint is not the fuSOL mint/,
    ));
  it("FAIL: canonical_primary != 1", () =>
    assert.match(
      checkMarketOracle(oracle({ canonical: 0 }), exp)!,
      /canonical_primary 0 != 1/,
    ));
  it("FAIL: haircut out of [1, 2000]", () => {
    assert.match(
      checkMarketOracle(oracle({ haircut: 0 }), exp)!,
      /liquidity_haircut_bps 0 out of/,
    );
    assert.match(
      checkMarketOracle(oracle({ haircut: 2001 }), exp)!,
      /liquidity_haircut_bps 2001 out of/,
    );
  });
  it("FAIL: lst_stake_pool not the fork StakePool", () =>
    assert.match(
      checkMarketOracle(oracle({ lst: OTHER }), exp)!,
      /lst_stake_pool is/,
    ));
  it("FAIL: pyth_feed_id != PYTH_SOL_USD_FEED_ID", () =>
    assert.match(
      checkMarketOracle(oracle({ pyth: Buffer.alloc(32) }), exp)!,
      /pyth_feed_id != PYTH_SOL_USD_FEED_ID/,
    ));
  it("FAIL: a DEX pool is bound", () => {
    assert.match(
      checkMarketOracle(oracle({ orca: OTHER }), exp)!,
      /orca_pool is set/,
    );
    assert.match(
      checkMarketOracle(oracle({ raydium: OTHER }), exp)!,
      /raydium_pool is set/,
    );
  });
  it("FAIL: missing account", () =>
    assert.match(checkMarketOracle(null, exp)!, /missing/));
});

describe("verify-fusol-deployment / check 9b — fuSOL Market", () => {
  const exp = { fusdCoreProgram: FUSDCORE, fusolMint: MINT };
  const market = (
    o: Partial<{
      owner: PublicKey;
      size: number;
      disc: Buffer;
      collateralMint: PublicKey;
      debtCeiling: bigint;
      flags: number;
    }> = {},
  ): Acct => {
    const w = new W(MARKET_SPACE);
    w.bytes(o.disc ?? anchorDiscriminator("Market"));
    w.pk(o.collateralMint ?? MINT); // @8
    w.buf.writeBigUInt64LE(o.debtCeiling ?? 1_000_000n, 170); // debt_ceiling @170
    w.buf.writeUInt8(o.flags ?? LIQ_INFRA_READY_MASK, 500); // liq_infra_flags @500
    return acct(o.owner ?? FUSDCORE, resize(w.buf, o.size));
  };
  it("good passes (ready = reactor pool + insurance buffer bits)", () => {
    assert.equal(checkMarket(market(), exp), null);
    assert.equal(
      checkMarket(market({ flags: LIQ_INFRA_READY_MASK | 1 }), exp),
      null,
    ); // bit0 also set
  });
  it("FAIL: wrong owner / size / discriminator", () => {
    assert.match(
      checkMarket(market({ owner: OTHER }), exp)!,
      /not the fusd-core program/,
    );
    assert.match(checkMarket(market({ size: 400 }), exp)!, /size 400 != 510/);
    assert.match(
      checkMarket(market({ disc: Buffer.alloc(8) }), exp)!,
      /wrong anchor discriminator/,
    );
  });
  it("FAIL: collateral_mint not fuSOL", () =>
    assert.match(
      checkMarket(market({ collateralMint: OTHER }), exp)!,
      /collateral_mint is not the fuSOL mint/,
    ));
  it("FAIL: debt_ceiling 0", () =>
    assert.match(
      checkMarket(market({ debtCeiling: 0n }), exp)!,
      /debt_ceiling is 0/,
    ));
  it("FAIL: liq infra not ready (only the born-gated bit0)", () => {
    assert.match(checkMarket(market({ flags: 0 }), exp)!, /not ready/);
    assert.match(checkMarket(market({ flags: 1 }), exp)!, /not ready/); // LIQ_INFRA_GATED alone
    assert.match(
      checkMarket(market({ flags: LIQ_INFRA_REACTOR_POOL }), exp)!,
      /not ready/,
    ); // one infra bit only
    assert.match(
      checkMarket(market({ flags: LIQ_INFRA_INSURANCE_BUFFER }), exp)!,
      /not ready/,
    );
  });
  it("FAIL: missing account", () =>
    assert.match(checkMarket(null, exp)!, /missing/));
});

describe("verify-fusol-deployment / config validation + PDA derivation gate", () => {
  // A well-formed config built from the REAL derived PDAs (so derivedPdaErrors is empty).
  const stakePool = KEY(60);
  const fusdCore = KEY(61);
  const fusolMint = KEY(62);
  const goodCfg = () => ({
    rpcUrl: "http://127.0.0.1:8899",
    phase: "sealed",
    fusolMint: fusolMint.toBase58(),
    maintenanceVault: KEY(63).toBase58(),
    stakePool: stakePool.toBase58(),
    validatorList: KEY(64).toBase58(),
    reserveStake: KEY(65).toBase58(),
    controllerConfig: deriveControllerConfig().toBase58(),
    poolAuthority: derivePoolAuthority().toBase58(),
    depositAuthority: deriveDepositAuthority().toBase58(),
    maintenanceAuthority: deriveMaintenanceAuthority().toBase58(),
    poolWithdrawAuthority: derivePoolWithdrawAuthority(stakePool).toBase58(),
    fusdCoreProgram: fusdCore.toBase58(),
    fusolMarket: PublicKey.findProgramAddressSync(
      [Buffer.from("market"), fusolMint.toBuffer()],
      fusdCore,
    )[0].toBase58(),
    programs: {
      controller: {
        expectUpgradeAuthority: "none",
        expectedElfSha256: "a".repeat(64),
      },
      fork: { expectUpgradeAuthority: KEY(70).toBase58() },
    },
  });

  it("validateConfig accepts a well-formed config", () => {
    assert.doesNotThrow(() => validateConfig(goodCfg()));
  });
  it("validateConfig rejects malformed configs", () => {
    assert.throws(() => validateConfig({ ...goodCfg(), rpcUrl: "" }), /rpcUrl/);
    assert.throws(
      () => validateConfig({ ...goodCfg(), phase: "later" }),
      /phase must be/,
    );
    assert.throws(
      () => validateConfig({ ...goodCfg(), fusolMint: "not-base58!!" }),
      /not a valid base58 pubkey/,
    );
    assert.throws(
      () => validateConfig({ ...goodCfg(), stakePool: 123 as any }),
      /must be a base58 string/,
    );
    const noPrograms: any = goodCfg();
    delete noPrograms.programs;
    assert.throws(
      () => validateConfig(noPrograms),
      /programs block is required/,
    );
    const badAuth: any = goodCfg();
    badAuth.programs.fork.expectUpgradeAuthority = "nope";
    assert.throws(
      () => validateConfig(badAuth),
      /not "none" nor a valid pubkey/,
    );
    const badHash: any = goodCfg();
    badHash.programs.controller.expectedElfSha256 = "xyz";
    assert.throws(() => validateConfig(badHash), /32-byte hex/);
    const noFork: any = goodCfg();
    delete noFork.programs.fork;
    assert.throws(() => validateConfig(noFork), /programs.fork is required/);
  });
  it("derivedPdaErrors: an honest config derives clean; a config that LIES about a PDA fails", () => {
    const cfg = goodCfg() as any;
    validateConfig(cfg);
    assert.deepEqual(derivedPdaErrors(cfg), []);
    const liar = { ...cfg, poolAuthority: KEY(88).toBase58() };
    const errs = derivedPdaErrors(liar);
    assert.equal(errs.length, 1);
    assert.match(errs[0], /poolAuthority: config .* != derived/);
    // A lying withdraw-authority PDA (mis-derived for the wrong pool) also fails.
    assert.ok(
      derivedPdaErrors({ ...cfg, poolWithdrawAuthority: KEY(89).toBase58() })
        .length === 1,
    );
    assert.ok(
      derivedPdaErrors({ ...cfg, fusolMarket: KEY(90).toBase58() }).length ===
        1,
    );
    assert.ok(
      derivedPdaErrors({ ...cfg, controllerConfig: KEY(91).toBase58() })
        .length === 1,
    );
  });
});
