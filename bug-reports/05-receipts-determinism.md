# TLDR

**Grade: C-.** The cryptographic primitives are correct and the trace-fingerprint / replay path works, but the headline pitch ("`heso open` produces signed, replayable receipts") is **false today** — no CLI verb produces a `Receipt`, and the only way to obtain one is via library-level Rust code. `receipt-verify` exists but has no upstream producer; `replay` works off `action-hash` JSON (a different artifact) which is keyless and unsigned. Determinism is solid for static pages, brittle for dynamic ones, and there is no recording/cassette mode despite ADR 0008 saying there should be.

---

# Discovery

## What the CLI ships (verified by running `heso.exe`)

The receipt/identity/replay subsystem in the CLI consists of seven commands plus one shared concept:

| Command                       | What it does                                                                                        |
|---                            |---                                                                                                  |
| `heso identity init [--path]` | Generates an Ed25519 keypair, writes the 32-byte raw seed to a file (default `heso-local-data/identity.key`), prints JSON `{path, public_key, algorithm}` on stdout. |
| `heso identity show [--path]` | Loads a keyfile, prints `{path, public_key, algorithm}`.                                            |
| `heso receipt-verify <file>`  | Reads a `Receipt` JSON, verifies its embedded Ed25519 signature. Exit 0/1/2 = valid/invalid/missing-or-malformed. |
| `heso action-hash <url> [json|-]` | Computes a keyless `TraceFingerprint` `{algorithm, url, actions, site_id, action_ids[], trace_id, canonical}` over (URL, actions). Prints JSON on stdout. |
| `heso action-hash-verify <file>`  | Recomputes the fingerprint, exit 0/1/2.                                                          |
| `heso replay <fp.json>`       | Re-executes the actions in a fingerprint against the live site. Refuses tampered files. Outputs a per-step session log. |
| `heso plat-hash` / `plat-verify`  | BLAKE3 over a plat (page) JSON file — content-addressing, a separate artifact.                  |

## What does NOT exist

Searching `crates/heso-cli/src` (with the Grep tool, not from memory) shows the CLI **never calls** `heso_trace_exec::run`, `heso_trace_exec::run_signed`, `heso_trace::sign_receipt`, or constructs a `Receipt` anywhere outside of tests. **There is no CLI verb that produces a Receipt.** The crate `heso-trace-exec` is listed as a `[dependencies]` entry in `crates/heso-cli/Cargo.toml:22` but the symbol is dead weight in the CLI binary.

Specifically, `heso open` (the verb the README points agents at) returns a `{url, title, tree, actions, plat_hash, …}` JSON document with **no `trace_hash`, no `signature`, no `seed`, no `mode`, no `cost`**. Confirmed by `jq` on `heso open https://example.com/` output.

There is also no `heso sign-receipt`, no `heso run`, and no way to ask any existing verb to "also emit a signed receipt of this call". This is a major gap given the README/ADR framing.

## Where the schema lives in code

- `crates/heso-trace/src/lib.rs` — `Receipt` struct (lines 106-143), `canonical_receipt_json` (635), `sign_receipt` (740), `verify_receipt` (770), `Mode` (44), `TraceFingerprint` (260), `Action` enum (469).
- `crates/heso-core/src/identity.rs` — `IdentityKey` (59), `Signature` envelope (186), `verify_strict` path.
- `crates/heso-trace-exec/src/lib.rs` — `run` and `run_signed` (the producer side; not wired to any CLI verb).
- `crates/heso-cli/src/main.rs:4789-4827` — `cmd_receipt_verify` (consumer-only).

---

# Round-trip results

## End-to-end attempt with `heso open`

```
$ cd bug-reports/scratch
$ heso open https://example.com/ > open.json
$ jq 'keys' open.json
[
  "actions", "console_errors_count", "description", "failed_scripts",
  "metadata", "partial", "partial_reason", "plat_hash", "title", "tree"
]
```

No `trace_hash`, no `signature`, no `seed`, no `mode`, no `cost`. There is nothing to feed to `heso receipt-verify`.

```
$ heso receipt-verify open.json
`open.json` is not a valid Receipt JSON: missing field `trace` at line 76 column 1
exit=2
```

**The `heso open` -> `heso receipt-verify` pipeline does not exist.** This is the central failure of the pitch.

## End-to-end attempt with `heso action-hash` -> `heso replay` (substitute path)

This path *does* round-trip:

```
$ heso action-hash https://example.com/ '[{"verb":"open","url":"https://example.com/"},{"verb":"click","ref":"@e0"}]' > fp.json
$ heso action-hash-verify fp.json
OK heso-trace-fp/v1 0a8dc39299b4e9b3f408e9769da237e57ea2c01d8dc693f9a19f184d2c9265ff
exit=0
$ heso replay fp.json | jq '.ok, .steps_run, .final_url'
true
2
"http://www.iana.org/help/example-domains"
```

Works end-to-end. But note: this is *keyless* — there is no signature, no identity, no "agent X did Y". A fingerprint is tamper-evident (you can't change `actions[]` without invalidating the IDs) but it's not *signed*. Anyone could produce a byte-identical fingerprint for the same (URL, actions).

## End-to-end signed-receipt round-trip (only possible via library API)

To prove the signed-receipt path works at all, I had to write a 30-line Cargo bin that imports `heso_trace::sign_receipt` directly (sources at `bug-reports/scratch/sign_helper/`). Output:

```
$ ./sign_helper signed_receipt.json heso-local-data/identity.key
wrote signed_receipt.json; pubkey=o5T/LJ2x9lktx2V5g3MP5CQKXPBITMirTaXPGndZglw=

$ heso receipt-verify signed_receipt.json
OK o5T/LJ2x9lktx2V5g3MP5CQKXPBITMirTaXPGndZglw=
exit=0
```

So `sign -> verify` works at the library level. There is just no shipping CLI command that performs the `sign` half.

## Replay reproducibility

I did **not** find a "replay reproduces byte-identical output" guarantee anywhere. The `replay` source comment explicitly says:

> "For byte-identical replay against recorded network responses, see ADR 0008 — designed, not yet implemented."

So today's replay re-executes the same actions against the *live* site; you get a fresh per-step session log, not a byte-identical reproduction of a prior session.

---

# Determinism results

## Static pages: byte-identical across runs

Three back-to-back runs of `heso open https://example.com/` produced 1604-byte output files, **diff returns zero output** — exactly identical bytes. Same for `https://en.wikipedia.org/wiki/Cat` (1,106,891 bytes, identical), `https://developer.mozilla.org/en-US/docs/Web/JavaScript` (178,760 bytes, identical), `https://news.ycombinator.com/item?id=39538886` (9,958 bytes, identical), and `heso read https://example.com/` (2286 bytes, identical).

So when the upstream HTML is stable, heso's engine is deterministic — no clock/RNG/ordering leaks from heso's own code. Good.

## Dynamic pages: server-side nondeterminism leaks straight through

`heso open https://github.com/torvalds/linux` is **not deterministic** across runs. The first run produced 153,158 bytes; the second produced 153,163 bytes. The diff:

```
56c56
<         "aria-labelledby": "tooltip-2446ac23-7ac6-481d-8430-6e4667e583d4",
---
>         "aria-labelledby": "tooltip-93e8dd11-c9ef-4819-83f8-66442b20394f",
58c58
<         "id": "icon-button-74b94e66-8fab-40f4-90ea-fda2bb6133e7",
---
>         "id": "icon-button-5b6f3f07-1bdc-4be4-8eeb-dff4c9ad3b84",
786c786
<         "aria-describedby": "validation-3769e0a2-c905-42db-ab8a-a8870f2e306b",
---
>         "aria-describedby": "validation-de974846-72d8-4b59-b189-035a6c82e608",
... etc ...
```

These are GitHub server-rendered UUIDs (per-request entropy on the server). The engine echoes them into its output and into `plat_hash`:

```
$ jq -r '.plat_hash' gh1.json gh2.json
8263f243979c2d84659c28458d78d639ae50a70c6c47183b93b759e17d79d3ae
2924c441513ed0ab9da9e8dd8cadec8abe5550c04db37ebb998a9b3ca828208c
```

Different `plat_hash` for the "same" page. This is exactly what the cassette / recording mode in ADR 0008 would fix (record on first run, replay deterministically against the recorded bytes thereafter), and that mode is **not implemented**. Until it is, "content addressing" via `plat_hash` is brittle on any page that contains server-side entropy — which is most modern sites.

This is NOT an engine bug — it is an architecture gap. heso's claim is "deterministic by default"; the reality is "deterministic against deterministic input." The promised cassette layer that would close that gap is absent.

---

# Schema

## Receipt JSON shape (from the library)

```json
{
  "trace":       [ { "op": "cd", "target": { "kind": "url", "url": "..." } }, ... ],
  "results":     [ { "op": "cd", "url": "..." }, ... ],
  "pages_seen":  [ "<64-hex-blake3>", ... ],     // optional, skipped if empty
  "trace_hash":  "<64-hex-blake3 of canonical(trace)>",
  "planner_id":  "<string>",                      // optional, skipped if empty
  "seed":        0,
  "mode":        "deterministic" | "recording" | "live",
  "cost":        { "bytes": 0, "cpu_ms": 0, "wall_ms": 0, "planner_tokens": 0 },
  "failed_at":   null,                            // optional
  "error":       null,                            // optional
  "signature":   {                                // optional; absent on unsigned receipts
    "algorithm": "Ed25519",
    "public_key": "<base64 32 bytes>",
    "signature":  "<base64 64 bytes>"
  }
}
```

Source: `crates/heso-trace/src/lib.rs:107-143` (`Receipt` struct) + `crates/heso-core/src/identity.rs:186-194` (`Signature`). Canonicalization rules at `canonical_receipt_json` (`lib.rs:635-646`) — sort object keys lexicographically, compact, escape strings the same way as `serde_json::to_string`, force `signature: null` before signing.

The signed payload is exactly `canonical_receipt_json(receipt)` (signature field nulled out), UTF-8 bytes. This is a clean, JSON-canonicalization-scheme-ish format. It is a *subset* of RFC 8785 — the doc comment is honest about this.

## TraceFingerprint JSON shape (the OTHER receipt-like artifact)

```json
{
  "algorithm":   "heso-trace-fp/v1",
  "url":         "<normalized URL>",
  "actions":     [ <user-supplied JSON array, opaque> ],
  "site_id":     "<64-hex-blake3 of normalized URL, site-domain>",
  "action_ids":  [ "<64-hex-blake3 per action>", ... ],
  "trace_id":    "<64-hex-blake3 chain of site + actions>",
  "canonical":   "<canonical-JSON of {actions, url}>"
}
```

## Could a third party verify?

For a **signed Receipt**: yes — receipt JSON + heso binary alone is sufficient, because the receipt embeds the pubkey. Verified:

```
$ cp signed_receipt.json /tmp/3rdparty_receipt.json
$ cd /tmp && heso receipt-verify 3rdparty_receipt.json
OK o5T/LJ2x9lktx2V5g3MP5CQKXPBITMirTaXPGndZglw=
```

But because the pubkey is *in* the receipt, **`receipt-verify` cannot tell a real receipt from a forgery by an attacker who simply generated their own key**. The verifier confirms "*someone* signed this thing"; it does not confirm "Akshay signed this." A `--expect-pubkey <b64>` flag (or a pubkey allowlist file) is the missing trust anchor, and it doesn't exist.

## Is the schema documented anywhere?

- `README.md` (284 lines): zero matches for `receipt`, `signed`, `Ed25519`, `signature`, `identity`. The pitch isn't mentioned in the README.
- No `.md` file in the repo contains the strings `receipt-verify`, `identity init`, `identity show`, `action-hash`, or `heso replay`.
- ADR 0005 says identity keys live at `~/.heso/identity/<agent-id>/key.priv`; the *code* stores them at `heso-local-data/identity.key`. ADR-vs-code **drift** (see "Bug list" below).
- The only places the schema is laid out are: the `Receipt` doc comment in `lib.rs`, and the `Action` doc comment in `lib.rs`. Rust internal.

---

# Crypto sanity

## Crate choices (verified in Cargo.lock)

- `ed25519-dalek 2.2.0` — current, audited.
- `ed25519 2.2.3` — current.
- `base64 0.22.1` — current.
- `blake3 1.8.5` — current.
- `rand_core 0.6.4` / `0.9.5` — current.

Reputable, in-active-maintenance choices. `IdentityKey::verify` (and `Signature::verify`) use `VerifyingKey::verify_strict`, which **does the weak-key check on top of plain Ed25519** — the right call given the pubkey is attacker-controlled at verify time. Good defense.

## Tampering tests (every one I tried rejects)

Starting with a valid signed receipt, then:

| Tamper                                            | Outcome (exit / msg)                                             |
|---                                                |---                                                               |
| `seed: 0` -> `seed: 999`                          | `INVALID: signature verification failed` (exit 1)                |
| `trace_hash: <hash>` -> `trace_hash: "0"*64`      | `INVALID: signature verification failed` (exit 1)                |
| Flip one byte inside the base64 signature         | `INVALID: signature verification failed` (exit 1)                |
| `algorithm: "Ed25519"` -> `"RSA"`                 | `INVALID: unsupported signature algorithm "RSA"` (exit 1)        |
| Remove `signature` field entirely                 | `MISSING: receipt has no "signature" field` (exit 2)             |
| Garbage JSON                                      | `not a valid Receipt JSON: ...` (exit 2)                         |
| Missing file                                      | `failed to read ...: cannot find file` (exit 2)                  |

Verify path is solid.

## Key storage

- Default path: `<cwd>/heso-local-data/identity.key`.
- Format: **32 raw bytes** of the Ed25519 seed. No envelope, no header. (`identity.rs:9-25`.)
- Windows permissions: NOT tightened. Code comment is honest about this: "Windows ACLs are not a one-line chmod. The directory is gitignored and the file is only readable by the user account anyway under default NTFS permissions. A follow-up can wire `cacls`-equivalents per platform." On Unix the file is `chmod 600`. (`identity.rs:139-148`.)
- Refuses to overwrite an existing keyfile (`save` returns `AlreadyExists`). Good.
- The key is *per-machine* (per-cwd, actually — `heso identity init` makes a fresh key in whatever directory you run it from). There is no notion of "the agent's identity" beyond a path-on-disk.

## A live-mode signed receipt verifies despite ADR 0005 / source comment

The source on `Mode::Live` (`lib.rs:53-55`) says "*No guarantees. Identity refuses to sign in this mode (M4).*" The corresponding code path doesn't actually enforce this. I signed a `mode: live` receipt via my helper and ran `heso receipt-verify`:

```
$ heso receipt-verify signed_live.json
OK o5T/LJ2x9lktx2V5g3MP5CQKXPBITMirTaXPGndZglw=
exit=0
```

So either the comment is wrong ("M4, not now") or the check is missing. Likely the former — but the receipt-verify side should reject `mode: live` too, otherwise the comment is just aspiration.

---

# Documentation grade

**Grade: D.**

- The README never mentions `receipt`, `identity`, `signed`, `signature`, or `Ed25519`. The differentiator pitch is invisible to anyone reading the README.
- `heso.exe` no-args output lists the commands but gives no example of producing a receipt — because no command produces one.
- `--help` is broken — `heso receipt-verify --help` tries to open a file called `--help`, fails with "cannot find file" and exit 2. Same for `replay`, `action-hash-verify`. `identity --help` returns "unknown identity subcommand: --help".
- ADR 0005 drifted hard from reality: claims paths under `~/.heso/identity/<agent-id>/`, claims a `heso-identity` crate, claims a `heso-audit` crate, claims an append-only signed-chain audit log. None of those exist. The actual implementation lives in `heso-core/src/identity.rs` and `heso-trace/src/lib.rs` at `heso-local-data/identity.key`. No audit log of any kind exists.
- The only place a new user would learn the schema is by reading the Rust source. There is no `docs/receipts.md`, no JSON example in the README, no man page.
- The `replay` command's docstring (output JSON `note` field, 530+ chars) is by far the densest user-facing documentation, and it's a runtime string — you only see it after a successful replay.

The bones are sound (doc comments on every public item, ADR exists for the crypto choice), but a new user cannot answer "how do I get a signed receipt?" from any user-facing doc, because the honest answer is "you can't from the CLI."

---

# Bug list

| Severity | Issue | Repro |
|---|---|---|
| **P0** | No CLI verb produces a `Receipt`. The signed-receipt pitch is unreachable from the CLI. `heso open / read / find / cat / ...` all return JSON with no `signature`, `trace_hash`, `seed`, or `mode` fields. The crate `heso-trace-exec` is depended on by `heso-cli` but never called. | `jq 'keys' <(heso open https://example.com/)` -> no signature/receipt fields. Grep `crates/heso-cli/src` for `trace_exec`/`run_signed`/`sign_receipt`/`Receipt {` -> zero matches. |
| **P0** | ADR 0005 has severe doc drift (key paths, crate names, audit log). A reader following the ADR will look for files that don't exist. | Read `decisions/0005-ed25519-identity.md` lines 26-32 (`~/.heso/...`, `heso-identity` crate, `heso-audit` crate, `audit.log`); none exist. Reality is `heso-local-data/identity.key` and a `heso-core::identity` module. |
| **P0** | README and all docs ship zero mention of the receipts/identity/replay subsystem despite "signed receipts" being the differentiator pitch. | `rg -i 'receipt\|signed\|ed25519\|signature\|identity' README.md` -> no matches. `rg 'receipt-verify\|identity init\|identity show\|action-hash\|heso replay' -g '*.md'` across the repo -> no matches. |
| **P1** | `receipt-verify` accepts ANY pubkey. There is no trust anchor — an attacker who generates their own key and signs a receipt produces an "OK" exit-0. The verify message even prints the attacker's pubkey as if it were legitimate. | `heso identity init --path other.key; sign_helper signed_by_other.json other.key; heso receipt-verify signed_by_other.json` -> `OK iGkB...`. Reproduced. |
| **P1** | No recording / cassette mode despite ADR 0008. `replay` re-runs against the live site; byte-identical replay is not implemented (the source comment admits this). | `crates/heso-cli/src/main.rs:4148-4151` says "For byte-identical replay against recorded network responses, see ADR 0008 — designed, not yet implemented." Grep for `cassette\|recording mode\|record_replay` -> structural absence. |
| **P1** | `Mode::Live` signed receipts verify "OK" even though the source comment and ADR 0005 say identity must refuse to sign in `Live` mode. The refusal isn't implemented anywhere. | Helper signs a `mode: live` receipt; `heso receipt-verify signed_live.json` -> `OK <pubkey>; exit=0`. Comment at `crates/heso-trace/src/lib.rs:53-55` says otherwise. |
| **P1** | Dynamic pages (e.g. github.com) leak server-side entropy (per-request UUIDs in attributes) into `heso open` output, breaking byte-identical determinism and the `plat_hash` content-addressing claim. | Two consecutive `heso open https://github.com/torvalds/linux` runs differ by ~20 attribute lines; `plat_hash` differs (`8263f24...` vs `2924c44...`). Static pages (example.com, wikipedia.org/Cat, MDN, HN item) are byte-identical. |
| **P2** | `--help` is broken on receipt-y commands. `heso receipt-verify --help` tries to open a file named `--help` and prints `cannot find file`. Same for `replay --help`, `action-hash-verify --help`. `identity --help` says "unknown subcommand". | `heso receipt-verify --help` -> `failed to read '--help': The system cannot find the file specified. (os error 2)`. |
| **P2** | The fingerprint vs receipt naming is confusing in the CLI. `action-hash` produces a `TraceFingerprint` ("trace_id"), `receipt-verify` reads a `Receipt` ("trace_hash"). They are distinct artifacts with distinct algorithms and distinct JSON shapes. Nothing surfaces this in `heso --help`. | Run `heso --help`; observe `action-hash` and `receipt-verify` listed near each other but no explanation of what they output relative to each other. |
| **P2** | `replay` is silently *not* byte-identical. The 530-char `note` in the output JSON warns about it, but this is only visible after a successful run. The command's `--help`-equivalent doesn't mention this caveat upfront. | `heso replay fp.json | jq -r .note` shows the caveats; `heso replay` with no args only gives short usage. |
| **P2** | Identity key file gets default-NTFS perms on Windows (intentional, documented in source) but is invisible to a security-conscious user — no warning at `identity init` time, no `.gitignore` reminder. The default path (`heso-local-data/`) IS in `.gitignore`, but if the user moves the key with `--path` to another spot, no warning. | `heso identity init --path mykey.key` in any non-gitignored dir succeeds quietly. |

---

# Top 5 polish items for the receipts story

These are ordered by leverage (impact-per-engineering-hour), not severity.

1. **Wire `heso-trace-exec::run_signed` into a real CLI verb.** Either (a) make `heso open` accept `--sign` and emit a Receipt-shaped envelope with `{trace, results, plat_hash, signature}`, or (b) ship a dedicated `heso sign <file>` that turns an existing `open` JSON into a signed envelope. Without this, *the signed-receipts pitch is vaporware from the CLI side.* The producer code exists; it just isn't called.

2. **Add a `--expect-pubkey <b64>` flag to `receipt-verify`.** Today the verifier confirms "someone signed this" — not "*who* signed it". A pubkey allowlist (file with one b64 per line, like SSH `authorized_keys`) plus an `--expect-pubkey` flag would close the trust anchor gap. Tiny code change; huge semantic improvement.

3. **Document the receipts subsystem in the README.** Three paragraphs: (a) "what a receipt is", (b) "what `heso identity init` does", (c) one full sample receipt JSON with field-by-field explanation. The most common failure mode of crypto features is "user can't figure out how to use them"; right now nothing in the README would teach a new user this exists at all.

4. **Ship the cassette / recording mode (ADR 0008).** Two new commands, `heso record <fp.json> -> cassette.json` (live fetch, persist responses) and `heso replay --cassette cassette.json` (no network, replay from disk). This is what makes determinism *real* on dynamic sites — without it, every claim about reproducibility is conditional on "the upstream HTML didn't change between runs," which it always does for modern sites.

5. **Reject `mode: live` in `verify_receipt` (and in `sign_receipt`).** Either honor the existing comment / ADR ("identity refuses to sign in this mode") or remove the comment. Pick one. Two-line code change.

A sixth that I almost put in: rewrite ADR 0005 to match the code. Current ADR mentions `~/.heso/identity/<agent-id>/`, a `heso-identity` crate, a `heso-audit` crate, and a chained audit log — none of which exist. A reader following the ADR will write code that targets vapor. Either the ADR is a *plan* (then mark it "partially implemented, see ADR 00XX for the actual shipped form") or it's a *spec* (then ship the rest).

---

# Appendix: artifacts

All scratch files (signed receipts, tampered receipts, sign helper, fingerprints, run-to-run diffs) are under `bug-reports/scratch/` for follow-up. Notable files:

- `bug-reports/scratch/signed_receipt.json` — a valid signed Receipt produced via the helper.
- `bug-reports/scratch/signed_by_other.json` — same receipt signed by a different key; `receipt-verify` returns "OK" anyway (P1 trust anchor bug).
- `bug-reports/scratch/signed_live.json` — `mode: live` signed receipt; `receipt-verify` accepts it (P1 mode bug).
- `bug-reports/scratch/tampered_*.json` — four flavors of tampering; all reject with exit 1.
- `bug-reports/scratch/fp_actions.json` and `fp_tamper.json` — clean and tampered fingerprints.
- `bug-reports/scratch/gh{1,2}.json` — two `heso open https://github.com/torvalds/linux` runs showing UUID-attribute drift.
- `bug-reports/scratch/sign_helper/` — minimal Cargo bin that imports `heso_trace::sign_receipt` directly, used to produce the signed test receipts.
