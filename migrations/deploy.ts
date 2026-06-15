// Anchor migration entrypoint (`anchor migrate`, run after `anchor deploy`).
//
// Protocol/market initialization lives in `scripts/bootstrap.ts` — the idempotent orchestrator that
// runs init_protocol → init_governance_gate → per-market market/oracle/reactor_pool/insurance_buffer.
// Run it after deploy:
//   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=<wallet> npx ts-node scripts/bootstrap.ts [config.json]
//
// Kept a no-op so `anchor deploy` never fires a half-configured init; wire bootstrap in here once the
// launch config (governance authority / guardian, per-market params) is finalized.
import * as anchor from "@coral-xyz/anchor";

module.exports = async function (provider: anchor.AnchorProvider) {
  anchor.setProvider(provider);
};
