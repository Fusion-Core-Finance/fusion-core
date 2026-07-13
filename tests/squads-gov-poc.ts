// This document describes an optional governance integration explored during development. Fusion
// Core does not depend on MetaDAO, futarchy or Squads. Any compatible signer or signer PDA may
// serve as the GovernanceGate inbound authority.
/**
 * Squads → fUSD governance PoC (optional integration).
 *
 * Demonstrates ONE possible external governance stack end-to-end against the bounded
 * GovernanceGate + timelock: a **Squads V4 vault PDA** (the gate's migratable `inbound_authority`)
 * can QUEUE a clamped fUSD parameter change via `vault_transaction_execute`, fUSD's
 * `require_keys_eq!(authority, gate.inbound_authority)` accepts it, and the change then applies via
 * the permissionless `execute_param_change`. This exercises the cross-program account ordering (the
 * Squads SDK compiles the message + execute remaining-accounts; fUSD accepts the vault-PDA
 * signature). The core validates only the signer/PDA — any upstream decision mechanism that
 * resolves to a compatible signer works identically; e.g. a futarchy layer such as MetaDAO's sits
 * ON TOP of Squads (`finalize_proposal` just CPIs `proposal_approve`) and is never visible to fUSD,
 * which only ever sees the vault-PDA signature at the gate. (Timelock TIMING is covered host-side
 * in `integration-tests/litesvm_governance.rs`; here the gate runs `timelock = 0` so the PoC stays
 * a single validator session.)
 *
 * Runs on its OWN validator via `scripts/run-squads-poc.sh` (the shared `config` singleton would
 * otherwise collide with `tests/fusd-core.ts`). Squads V4 (.so) + its ProgramConfig account come
 * from `fixtures/` (see `scripts/fetch-squads.sh`).
 */
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { assert } from "chai";
import * as multisig from "@sqds/multisig";
import { createMint, TOKEN_PROGRAM_ID } from "@solana/spl-token";
import { FusdCore } from "../target/types/fusd_core";

const { PublicKey, Keypair, SystemProgram, SYSVAR_RENT_PUBKEY, TransactionMessage, LAMPORTS_PER_SOL } =
  anchor.web3;
const BN = anchor.BN;
const RAY = new BN("1000000000000000000000000000"); // 1e27 = 0% per-second interest

describe("Squads→fUSD governance PoC (optional integration)", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.fusdCore as Program<FusdCore>;
  const connection = provider.connection;
  const signer = (provider.wallet as anchor.Wallet).payer; // bootstrap admin / executor / fee payer

  const pda = (seeds: (Buffer | Uint8Array)[]) =>
    PublicKey.findProgramAddressSync(seeds, program.programId)[0];
  const [config] = PublicKey.findProgramAddressSync([Buffer.from("config")], program.programId);
  const [govGate] = PublicKey.findProgramAddressSync([Buffer.from("gov_gate")], program.programId);
  const timelockPda = (nonce: number) =>
    pda([Buffer.from("timelock"), new BN(nonce).toArrayLike(Buffer, "le", 8)]);

  const createKey = Keypair.generate();
  const [multisigPda] = multisig.getMultisigPda({ createKey: createKey.publicKey });
  const [vaultPda] = multisig.getVaultPda({ multisigPda, index: 0 });

  let coll: anchor.web3.PublicKey;
  let market: anchor.web3.PublicKey;
  let txIndex = 0n;

  async function confirm(sig: string) {
    const bh = await connection.getLatestBlockhash();
    await connection.confirmTransaction({ signature: sig, ...bh }, "confirmed");
  }

  /** Drive an arbitrary fUSD instruction through the full Squads lifecycle, signed by the vault PDA. */
  async function executeViaSquads(instructions: anchor.web3.TransactionInstruction[]) {
    txIndex += 1n;
    const transactionMessage = new TransactionMessage({
      payerKey: vaultPda,
      recentBlockhash: (await connection.getLatestBlockhash()).blockhash,
      instructions,
    });
    await confirm(
      await multisig.rpc.vaultTransactionCreate({
        connection, feePayer: signer, multisigPda, transactionIndex: txIndex,
        creator: signer.publicKey, vaultIndex: 0, ephemeralSigners: 0,
        transactionMessage, sendOptions: { skipPreflight: true },
      })
    );
    await confirm(
      await multisig.rpc.proposalCreate({
        connection, feePayer: signer, creator: signer, multisigPda, transactionIndex: txIndex,
      })
    );
    await confirm(
      await multisig.rpc.proposalApprove({
        connection, feePayer: signer, member: signer, multisigPda, transactionIndex: txIndex,
      })
    );
    await confirm(
      await multisig.rpc.vaultTransactionExecute({
        connection, feePayer: signer, multisigPda, transactionIndex: txIndex,
        member: signer.publicKey, signers: [signer], sendOptions: { skipPreflight: true },
      })
    );
  }

  it("sets up Squads + a market + the GovernanceGate (inbound authority = vault PDA)", async () => {
    const programConfigPda = multisig.getProgramConfigPda({})[0];
    const programConfig = await multisig.accounts.ProgramConfig.fromAccountAddress(
      connection,
      programConfigPda
    );
    await confirm(
      await multisig.rpc.multisigCreateV2({
        connection, treasury: programConfig.treasury, createKey, creator: signer, multisigPda,
        configAuthority: null, threshold: 1,
        members: [{ key: signer.publicKey, permissions: multisig.types.Permissions.all() }],
        timeLock: 0, rentCollector: null, sendOptions: { skipPreflight: true },
      })
    );
    // The vault PDA pays rent for the TimelockedParam it creates when it QUEUES → fund it.
    await confirm(await connection.requestAirdrop(vaultPda, 100 * LAMPORTS_PER_SOL));

    // gov_authority = wallet (bootstrap admin: creates market + gate). The vault PDA is the gate's
    // inbound (param-tuning) authority.
    await program.methods
      .initProtocol({ govAuthority: signer.publicKey, guardian: signer.publicKey })
      .accounts({ payer: signer.publicKey, config, systemProgram: SystemProgram.programId })
      .rpc();

    coll = await createMint(connection, signer, signer.publicKey, null /* no freeze */, 9);
    market = pda([Buffer.from("market"), coll.toBuffer()]);
    await program.methods
      .initMarket({
        mcrBps: 15_000,
        debtCeiling: new BN(1_000_000).mul(new BN(1_000_000)),
        perSecRate: RAY,
        reserveLamports: new BN(0),
        liqGasCompBps: 0,
        bucketWidthBps: 10,
        redemptionFeeBps: 0,
      })
      .accounts({
        authority: signer.publicKey,
        config,
        collateralMint: coll,
        market,
        collateralVault: pda([Buffer.from("coll_vault"), coll.toBuffer()]),
        redemptionBitmap: pda([Buffer.from("redeem_bitmap"), coll.toBuffer()]),
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    // Gate: inbound authority = the Squads vault PDA; timelock 0 for the PoC.
    await program.methods
      .initGovernanceGate(vaultPda, new BN(0))
      .accounts({ authority: signer.publicKey, config, govGate, systemProgram: SystemProgram.programId })
      .rpc();

    const gate = await program.account.governanceGate.fetch(govGate);
    assert.ok(gate.inboundAuthority.equals(vaultPda), "gate inbound authority is the vault PDA");
  });

  it("queues a param change via a Squads proposal, then executes it (the end-to-end proof)", async () => {
    const queueIx = await program.methods
      .queueParamChange({ redemptionFee: {} }, new BN(123))
      .accounts({
        authority: vaultPda,
        govGate,
        market,
        timelockedParam: timelockPda(0),
        systemProgram: SystemProgram.programId,
      })
      .instruction();

    await executeViaSquads([queueIx]); // vault PDA signs the QUEUE via Squads

    // timelock 0 → permissionless execute applies it immediately.
    await program.methods
      .executeParamChange()
      .accounts({ executor: signer.publicKey, market, timelockedParam: timelockPda(0) })
      .rpc();

    const m = await program.account.market.fetch(market);
    assert.strictEqual(m.redemptionFeeBps, 123, "vault PDA moved the param through the gate");
  });

  it("rejects a non-vault signer queuing directly", async () => {
    const nonce = (await program.account.governanceGate.fetch(govGate)).queueNonce.toNumber();
    let threw = false;
    try {
      await program.methods
        .queueParamChange({ redemptionFee: {} }, new BN(200))
        .accounts({
          authority: signer.publicKey, // not the vault PDA
          govGate,
          market,
          timelockedParam: timelockPda(nonce),
          systemProgram: SystemProgram.programId,
        })
        .rpc();
    } catch (e) {
      threw = true;
      assert.match(String(e), /Unauthorized|6000/, "fails the inbound-authority check");
    }
    assert.ok(threw, "direct non-vault queue must be rejected");
    assert.strictEqual((await program.account.market.fetch(market)).redemptionFeeBps, 123);
  });
});
