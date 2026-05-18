# 0005. Ed25519 identity + signed audit log

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay

## Context

Agents acting on the web on a user's behalf raise real accountability questions:

- *Which* agent took *which* action *when*?
- Can the user prove an action was taken by their agent (vs. an impersonator)?
- Can a third party (an org, a partner, an auditor) verify an agent's action history?

Today, every agent framework handles this differently or not at all. There's no portable cryptographic identity for agents that travels across tools.

We want heso's identity story to be:

- Cryptographically real — not just an opaque ID string.
- Cheap and fast — no per-action chain transactions.
- Portable — the agent identity should be meaningful outside heso, eventually across agentware.
- Optional-on-chain — anchorable to Solana / Base for cross-org trust, but not required for local use.

## Decision

**Ed25519 keypair per agent instance, stored at `~/.heso/identity/<agent-id>/`.** Every action that crosses the heso API boundary is signed by the active agent's private key. The audit log is an append-only, signature-chained record (one hash links to the previous).

- Private key: `~/.heso/identity/<agent-id>/key.priv` (mode `0600`, never logged, never exported via API).
- Public key + DID document: `~/.heso/identity/<agent-id>/identity.json`.
- Audit log: `~/.heso/identity/<agent-id>/audit.log` (append-only, JSON-lines, each line signed).
- `heso-identity` crate handles key generation, signing, verification.
- `heso-audit` crate handles the log format, chaining, and verification.

**On-chain anchoring is optional** and behind a feature flag (`anchor-solana`, `anchor-base`). Off by default. Adds nothing to the local-use case; provides cross-org verifiability when enabled.

## Alternatives considered

- **No identity / no signing.** Rejected: gives up the agentware-differentiator pitch and the audit story.
- **Username + password / API key.** Rejected: not cryptographically meaningful, can't sign individual actions, doesn't compose with DIDs or any standards.
- **OAuth / OIDC integration only.** Rejected: ties identity to a provider, doesn't work offline, doesn't sign individual actions.
- **Always on-chain (every action recorded).** Rejected: expensive, slow, leaks data, requires gas funding for every user. Anchoring is fine; per-action is not.
- **JWT-style signed tokens (no chained log).** Rejected: gives signing but not tamper-evidence — an attacker with key access could rewrite history without detection.

## Consequences

**Positive:**
- Real cryptographic provenance for every action.
- Tamper-evident audit log (insertion / removal / reorder is detectable).
- Optional chain anchoring gives cross-org trust without forcing on-chain UX for normal use.
- Aligns with W3C DID standard for portability outside heso.

**Negative:**
- Key management is a real burden — backup, rotation, multi-device sync are non-trivial UX. We will need to design a recovery story.
- Compromised private key means an attacker can sign actions as that agent until rotated.
- Audit log on disk can grow large for long-running agents; need rotation / archival policy (separate ADR later).
- Anchoring to chains adds operational complexity if/when enabled.

## References

- [W3C DID Core](https://www.w3.org/TR/did-core/)
- [Ed25519 (RFC 8032)](https://datatracker.ietf.org/doc/html/rfc8032)
- ADR 0002 (engine trait boundary) — identity is enforced at the trait boundary so engines can't sidestep signing.
- `state.json` D-002 (identity storage format details — pending sub-ADR).
