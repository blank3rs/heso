# HESO/1.0

HESO/1.0 is an open protocol for **agent-driven web observation**. It
defines four interlocking data structures — the **plat** (a
content-addressed JSON observation of one web resource), the **cassette**
(the embedded record of every HTTP exchange the observation touched), the
**receipt** (a signed attestation of an executed action trace), and the
**verb namespace** (the canonical names agents use to act on the web) —
plus the **determinism rules** that let any conformant implementation
re-execute a plan and produce a byte-identical hash.

This file is a stub. **The canonical spec lives at <https://heso.ca/spec>.**

## Core verbs (HESO/1.0)

A conformant implementation MUST dispatch the following verbs. Detailed
wire format, JSON output shapes, exit-code semantics, and the four
plan-resident action verbs (`open`, `click`, `fill`, `submit`) are
specified on the canonical spec page.

| Verb | Role |
|---|---|
| `read` | Fetch + execute JS + return rich content (text, forms, cookies, console, framework, deltas). |
| `open` | Page summary (title, headings, action graph). |
| `click` | Dispatch a click on an element matched by ref / text / selector / aria. |
| `fill` | Set the value of an input and fire `input` + `change`. |
| `submit` | Serialize a form, POST per `enctype`, observe the response. |
| `stamp` | Execute a plan against the live web; mint a plat with embedded cassette. |
| `run` | Re-execute a plan against the embedded cassette — no network. |
| `replay` | Emit the recorded step log from a plat. With `--plan`, emit the standalone plan JSON. |
| `refresh` | Re-stamp a plat against the live web and report whether it has drifted. |
| `verify` | Polymorphic content-identity check across plats, sealed envelopes, and signed receipts. |
| `info` | Human summary of a plat (with two args, a structural diff). |
| `seal` | Wrap a plat in an Ed25519 envelope. |
| `unseal` | Verify a sealed envelope; with `--extract`, emit the inner plat body. |
| `eval-js` | Evaluate JS in a sandboxed QuickJS context with seeded entropy and a virtual clock. |
| `eval-dom` | Fetch a URL, run its scripts, then evaluate JS against the post-hydration DOM. |
| `wait` | Block until a page condition is satisfied. |
| `batch` | Run many URLs in parallel under one cookie jar. |
| `search` | Multi-backend web search across Mojeek, DuckDuckGo, and Wikipedia (optional SearXNG); no API key. |
| `serve` | Long-running JSON-RPC 2.0 server over stdin/stdout. |
| `identity` | Generate or inspect an Ed25519 signing identity. |

## Reference implementation

The reference implementation is the `heso` binary in this repository.
Dispatch behavior, flag surface, and JSON output shapes are defined by
[`crates/heso-cli/src/main.rs`](../crates/heso-cli/src/main.rs); plat,
cassette, and receipt construction live in the `heso-engine-*` crates
alongside it.

A second implementation in any language is sufficient to validate the
spec. The canonical spec at <https://heso.ca/spec> is the binding text;
this file exists so external references that resolve to
`spec/HESO-1.0.md` in the source tree continue to work.

## License

CC0 1.0 (spec text) · MIT or Apache-2.0 (reference implementation).
