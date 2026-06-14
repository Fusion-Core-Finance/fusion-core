import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { assert } from "chai";
import { FusdCore } from "../target/types/fusd_core";

const { PublicKey, Keypair, SystemProgram } = anchor.web3;

describe("fusd-core", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.fusdCore as Program<FusdCore>;

  it("init_protocol creates the config PDA", async () => {
    const [config] = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      program.programId
    );
    const govAuthority = Keypair.generate().publicKey;
    const guardian = Keypair.generate().publicKey;

    await program.methods
      .initProtocol({ govAuthority, guardian })
      .accounts({
        payer: provider.wallet.publicKey,
        config,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const cfg = await program.account.protocolConfig.fetch(config);
    assert.ok(cfg.govAuthority.equals(govAuthority), "gov_authority stored");
    assert.ok(cfg.guardian.equals(guardian), "guardian stored");
    assert.strictEqual(cfg.emergency, false, "emergency defaults false");
    assert.ok(cfg.deployer.equals(provider.wallet.publicKey), "deployer recorded");
  });
});
