// Anchor migration entrypoint (`anchor migrate`). Deploy/bootstrap logic — e.g. calling
// `init_protocol` with the launch governance authority + guardian — lands here as the
// deploy flow is finalized. See fusion-docs.md (phased roadmap).
import * as anchor from "@coral-xyz/anchor";

module.exports = async function (provider: anchor.AnchorProvider) {
  anchor.setProvider(provider);
  // TODO(deploy): init_protocol(gov_authority = Squads vault PDA, guardian = ...).
};
