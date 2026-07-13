/**
 * fUSD keys-sync — inspect and update the protocol's authority + oracle-program keys held in
 * ProtocolConfig ([b"config"]) and the GovernanceGate ([b"gov_gate"]):
 *   - the Pyth receiver program id (+ a second "alt" id for the ~2026-07-31 Pyth core migration)
 *     and the Switchboard on-demand program id — kept in sync via set_oracle_program_ids;
 *   - the two-step gov_authority handoff (migrate_gov_authority → accept_gov_authority);
 *   - the two-step gov-gate inbound authority handoff (migrate_inbound_authority → accept_inbound_authority).
 *
 * SAFETY: every mutating action is DRY-RUN by default — prints the instruction as
 * governance-proposal-ready JSON (e.g. for a multisig) and only signs + submits on --send.
 * set-oracle-ids + migrate-gov are signed by the current gov_authority; migrate-inbound by the current
 * inbound_authority; the accept-* steps by the NEW key itself. Use --authority <pubkey> in dry-run to
 * emit an instruction whose signer is an external signer/PDA (e.g. a multisig vault).
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=<authority.json> npx ts-node keepers/keys-sync.ts <cmd> [flags]
 *   show                                                  print all on-chain authority + oracle keys
 *   set-oracle-ids [--pyth <pk>] [--pyth-alt <pk>|clear] [--switchboard <pk>] [--send] [--authority <pk>]
 *   migrate-gov     --to <pk> [--send] [--authority <pk>]   propose a new gov_authority
 *   accept-gov                 [--send] [--authority <pk>]  the pending gov_authority accepts (signs)
 *   migrate-inbound --to <pk> [--send] [--authority <pk>]   propose a new gov-gate inbound authority
 *   accept-inbound             [--send] [--authority <pk>]  the pending inbound authority accepts (signs)
 */
import { makeProgram, PublicKey, Pk } from "./common";
import { flags, govPdas, sendOrPrint, authorityOf, printUsage, runCli, log } from "./gov-common";

const ZERO = PublicKey.default;
const fmtKey = (k: any): string => { const s = k.toBase58(); return s === ZERO.toBase58() ? `${s}  (unset/disabled)` : s; };

async function show(program: any, pid: Pk) {
  const g = govPdas(pid);
  const c: any = await program.account.protocolConfig.fetch(g.config);
  log(`ProtocolConfig ${g.config.toBase58()}`);
  log(`  gov_authority             ${fmtKey(c.govAuthority)}`);
  if (!c.pendingGovAuthority.equals(ZERO)) log(`  pending_gov_authority     ${c.pendingGovAuthority.toBase58()}  (awaiting accept-gov)`);
  log(`  guardian                  ${fmtKey(c.guardian)}`);
  log(`  deployer                  ${c.deployer.toBase58()}`);
  log(`  fusd_mint                 ${c.fusdMint.toBase58()}`);
  log(`  pyth_receiver_program_id  ${fmtKey(c.pythReceiverProgramId)}`);
  log(`  pyth_receiver_alt         ${fmtKey(c.pythReceiverProgramIdAlt)}`);
  log(`  switchboard_program_id    ${fmtKey(c.switchboardProgramId)}`);
  try {
    const gate: any = await program.account.governanceGate.fetch(g.govGate);
    log(`GovernanceGate ${g.govGate.toBase58()}`);
    log(`  inbound_authority         ${fmtKey(gate.inboundAuthority)}`);
    if (!gate.pendingInboundAuthority.equals(ZERO)) log(`  pending_inbound_authority ${gate.pendingInboundAuthority.toBase58()}  (awaiting accept-inbound)`);
    log(`  timelock_secs             ${gate.timelockSecs}`);
    log(`  queue_nonce               ${gate.queueNonce}`);
  } catch { log("GovernanceGate: not initialized"); }
}

async function main() {
  const cmd = process.argv[2];
  const f = flags(process.argv.slice(3));
  const send = f.has("send");
  if (!cmd || cmd === "help" || cmd === "--help") { printUsage(__filename); return; }

  const { program, pid, me } = makeProgram();
  const g = govPdas(pid);

  switch (cmd) {
    case "show": return show(program, pid);

    case "set-oracle-ids": {
      const parse = (flag: string, allowClear: boolean): Pk | null => {
        const v = f.get(flag); if (v === undefined) return null; // unchanged
        if (v === "clear") { if (!allowClear) throw new Error(`--${flag} cannot be cleared (a zero program id would brick the crank)`); return ZERO; }
        const pk = new PublicKey(v);
        if (!allowClear && pk.equals(ZERO)) throw new Error(`--${flag} must not be the zero pubkey (would brick the crank)`);
        return pk;
      };
      const pyth = parse("pyth", false), alt = parse("pyth-alt", true), sb = parse("switchboard", false);
      if (!pyth && !alt && !sb) throw new Error("set-oracle-ids needs at least one of --pyth / --pyth-alt / --switchboard");
      log(`set-oracle-ids: pyth=${pyth?.toBase58() ?? "(unchanged)"} alt=${alt ? (alt.equals(ZERO) ? "(clear)" : alt.toBase58()) : "(unchanged)"} switchboard=${sb?.toBase58() ?? "(unchanged)"}`);
      const b = program.methods.setOracleProgramIds(pyth, alt, sb).accounts({ authority: authorityOf(f, me, send), config: g.config });
      return sendOrPrint(b, "set-oracle-ids", send);
    }

    case "migrate-gov": {
      const to = new PublicKey(f.get("to") ?? (() => { throw new Error("--to <new gov authority pubkey> is required"); })());
      const b = program.methods.migrateGovAuthority(to).accounts({ authority: authorityOf(f, me, send), config: g.config });
      return sendOrPrint(b, `migrate-gov → ${to.toBase58()}`, send);
    }
    case "accept-gov": {
      const who = authorityOf(f, me, send); // --authority: dry-run the accept for an external signer/PDA (e.g. a multisig vault)
      const b = program.methods.acceptGovAuthority().accounts({ newAuthority: who, config: g.config });
      return sendOrPrint(b, `accept-gov as ${who.toBase58()}`, send);
    }
    case "migrate-inbound": {
      const to = new PublicKey(f.get("to") ?? (() => { throw new Error("--to <new inbound authority pubkey> is required"); })());
      const b = program.methods.migrateInboundAuthority(to).accounts({ authority: authorityOf(f, me, send), govGate: g.govGate });
      return sendOrPrint(b, `migrate-inbound → ${to.toBase58()}`, send);
    }
    case "accept-inbound": {
      const who = authorityOf(f, me, send); // --authority: dry-run the accept for an external signer/PDA (e.g. a multisig vault)
      const b = program.methods.acceptInboundAuthority().accounts({ newAuthority: who, govGate: g.govGate });
      return sendOrPrint(b, `accept-inbound as ${who.toBase58()}`, send);
    }

    default: throw new Error(`unknown command "${cmd}" — run: npx ts-node keepers/keys-sync.ts help`);
  }
}

if (require.main === module) runCli(main);
