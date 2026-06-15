/**
 * Fusion (FUSD) SDK — PDA derivation helpers.
 *
 * Seeds mirror `programs/fusd-core/src/constants.rs`. Account decoders and
 * instruction builders are added from the generated Anchor IDL as flows land.
 */
import { PublicKey } from "@solana/web3.js";

export const FUSD_CORE_PROGRAM_ID = new PublicKey(
  "FuSiontgYvCc2N2Cinvo5gxSuxt2UfGxKMcbzkB67kud"
);

export const SEEDS = {
  config: Buffer.from("config"),
  market: Buffer.from("market"),
  position: Buffer.from("position"),
  reactorPool: Buffer.from("reactor"),
  reactorDeposit: Buffer.from("reactor_dep"),
  dexTwap: Buffer.from("twap"),
  surplus: Buffer.from("surplus"),
  collSurplus: Buffer.from("coll_surplus"),
  rateLimiter: Buffer.from("ratelimit"),
  registry: Buffer.from("registry"),
  govGate: Buffer.from("gov_gate"),
  timelock: Buffer.from("timelock"),
  mintAuthority: Buffer.from("mint_authority"),
} as const;

export function deriveConfig(programId: PublicKey = FUSD_CORE_PROGRAM_ID): PublicKey {
  return PublicKey.findProgramAddressSync([SEEDS.config], programId)[0];
}

export function deriveMarket(
  collateralMint: PublicKey,
  programId: PublicKey = FUSD_CORE_PROGRAM_ID
): PublicKey {
  return PublicKey.findProgramAddressSync(
    [SEEDS.market, collateralMint.toBuffer()],
    programId
  )[0];
}

export function derivePosition(
  collateralMint: PublicKey,
  owner: PublicKey,
  programId: PublicKey = FUSD_CORE_PROGRAM_ID
): PublicKey {
  return PublicKey.findProgramAddressSync(
    [SEEDS.position, collateralMint.toBuffer(), owner.toBuffer()],
    programId
  )[0];
}
