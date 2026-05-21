# fix-04-receipts — P0 + 2× P1 bugs (signed receipts CLI wiring)

Date: 2026-05-21
Branch: `worktree-agent-a59f43f4808668db4`
Commits (bottom to top):

- `970d965` — `cli: --receipt flag on open/read signs the trace (P0 receipts fix)`
- `36b2101` — `cli: trusted-keys allowlist on receipt-verify (P1 trust anchor)`
- `c5aabf9` — `cli: reject mode=live in receipt-verify (P1 replay-safety)`
- `34bacae` — `test: end-to-end receipts round-trip via wiremock`
- `49436df` — `docs: README — full signed-receipts walkthrough`

## Design choice

I picked **Option C** from the audit (`--receipt PATH` flag on existing
verbs). The reasoning:

- The verb's existing stdout JSON shape is unchanged, so every existing
  pipeline (`heso open | jq`, agent harnesses, etc.) keeps working.
- The receipt lands at a caller-chosen path, not a sibling we'd have to
  invent a naming convention for.
- Option A (`--sign` flag) either mutates the stdout shape (breaking
  every existing consumer) or hides receipts in some "sensible default"
  file path (which has its own backwards-compat surface).
- Option B (new `record` wrapper verb) forces callers to learn a new
  top-level command and re-plumb every wrapper / framework adapter.

I also added the receipt suite to `open` and `read` but **not** to
`batch`. Batch has N URLs per call, so receipts there need either
`--receipt-dir` (one file per URL) or JSON-Lines, both of which are
their own design exercise. The single-URL verbs cover the headline
pitch cleanly; batch can land in a follow-up if we get demand.

## Files touched

| File | Diff | What |
|------|----:|------|
| `crates/heso-cli/src/receipts.rs` | +472 (new) | New module: `SignFlags`, `try_consume_sign_flag` (the shared `--receipt/--key/--mode/--seed` arg-walker), `url_trace` (constructs `vec![PrimitiveOp::Cd(CdInput{target:CdTarget::Url{url}})]`), `emit_signed_receipt` (loads key, builds `SessionConfig`, calls `heso_trace_exec::run_signed`, writes the pretty-printed receipt to disk), `AllowlistResult` + `load_trusted_keys` + `parse_allowlist_json` + `pubkey_in_allowlist`, `TRUSTED_KEYS_ENV` constant. 13 inline unit tests. |
| `crates/heso-cli/src/main.rs` | +175 / -8 | Register `mod receipts;`. Wire `--receipt`/`--key`/`--mode`/`--seed` into `cmd_open` and `cmd_read` arg-walk loops via `receipts::try_consume_sign_flag`. Call `receipts::emit_signed_receipt(&engine, &trace, &sign_flags)` right before each verb's `print_json(&body)`. Rewrite `cmd_receipt_verify` to (a) parse `--trusted-keys PATH`, (b) resolve `AllowlistResult` from flag or `HESO_TRUSTED_KEYS` env, (c) reject `mode: live` upfront, (d) gate the OK path on the allowlist when non-empty. Updated `print_banner` to document the new flags. |
| `crates/heso-cli/tests/receipts_round_trip.rs` | +378 (new) | 6 wiremock-backed integration tests — see "Test additions" below. |
| `README.md` | +69 / 0 | New "Signed receipts" section between the determinism example and error handling. Walks identity init → `--receipt` → verify with allowlist → 3 rejection modes (tamper, wrong signer, mode:live) → env-var fallback → exit-code semantics. |

Workspace test count after the change: 57 test result sections, all
green; 6 new integration tests pass against the release binary. The
new `receipts::tests` module adds 13 unit tests inside the heso bin
target.

## Working CLI flow

Exact commands, copy-pasteable, executed against the release binary
built at `target/release/heso.exe` on this branch:

```sh
# 1) One-time setup
$ mkdir demo && cd demo
$ heso identity init
{
  "algorithm": "Ed25519",
  "path": "heso-local-data/identity.key",
  "public_key": "GTy7akfRTH2C9CI8YCpZtx3tGCeHYq7/qee21mrrRD0="
}

# 2) Sign a receipt for an `open` call. stdout is the normal page JSON.
$ heso open https://example.com --receipt receipt.json
{
  "actions": [ ... ],
  ...
}
$ ls -la receipt.json
-rw-r--r-- 1 user 663 receipt.json
$ cat receipt.json
{
  "trace": [
    {"op": "cd", "target": {"kind": "url", "url": "https://example.com/"}}
  ],
  "results": [{"op": "cd", "url": "https://example.com/"}],
  "trace_hash": "7e501fac0af7c5849fcf829d104b2050efe73b383b883d5e54ac9c57bb9cf4be",
  "seed": 0,
  "mode": "deterministic",
  "cost": {"bytes": 0, "cpu_ms": 0, "wall_ms": 0, "planner_tokens": 0},
  "signature": {
    "algorithm": "Ed25519",
    "public_key": "GTy7akfRTH2C9CI8YCpZtx3tGCeHYq7/qee21mrrRD0=",
    "signature": "cPsGJQ5HFk44QYL1hbJDz4AMEPe49nl8DHEDAc7n1ci9dQhwJPdm6+ql2nYekt3cesPaytqoTL31Wl8xW5wCDw=="
  }
}

# 3) Verify with the correct allowlist
$ echo '["GTy7akfRTH2C9CI8YCpZtx3tGCeHYq7/qee21mrrRD0="]' > trusted.json
$ heso receipt-verify --trusted-keys trusted.json receipt.json
OK GTy7akfRTH2C9CI8YCpZtx3tGCeHYq7/qee21mrrRD0=
$ echo "exit: $?"
exit: 0

# 4) Tamper one byte (seed: 0 → 999), re-verify, see rejection
$ python -c "import json; d=json.load(open('receipt.json')); d['seed']=999; json.dump(d, open('tampered.json','w'), indent=2)"
$ heso receipt-verify --trusted-keys trusted.json tampered.json
INVALID: signature verification failed
$ echo "exit: $?"
exit: 1

# 5) Wrong-pubkey allowlist → rejection (P1 trust-anchor fix)
$ echo '["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]' > wrong.json
$ heso receipt-verify --trusted-keys wrong.json receipt.json
INVALID: signing pubkey `GTy7akfRTH2C9CI8YCpZtx3tGCeHYq7/qee21mrrRD0=` is not in the trusted-keys allowlist
$ echo "exit: $?"
exit: 1

# 6) mode: live receipt → rejection (P1 replay-safety fix, ADR 0008)
$ heso open https://example.com --receipt live.json --mode live
{ ... }
$ heso receipt-verify --trusted-keys trusted.json live.json
INVALID: receipt `mode: live` is not replay-safe — per ADR 0008, only `deterministic` and `recording` receipts can be verified (live runs use wall-clock time and real network, so the signature has no replay value)
$ echo "exit: $?"
exit: 1

# 7) No-allowlist verify still works, but warns to stderr so the
#    missing trust anchor isn't silent. (Legacy behavior preserved.)
$ heso receipt-verify receipt.json
warning: no pubkey allowlist configured (pass --trusted-keys PATH or set HESO_TRUSTED_KEYS to bind receipts to a known signer; verifying signatures without identity)
OK GTy7akfRTH2C9CI8YCpZtx3tGCeHYq7/qee21mrrRD0=
$ echo "exit: $?"
exit: 0

# 8) HESO_TRUSTED_KEYS env-var alternative
$ HESO_TRUSTED_KEYS=trusted.json heso receipt-verify receipt.json
OK GTy7akfRTH2C9CI8YCpZtx3tGCeHYq7/qee21mrrRD0=
$ echo "exit: $?"
exit: 0
```

## Test additions

`crates/heso-cli/tests/receipts_round_trip.rs` — 6 wiremock-backed
integration tests, each running in its own `TempDir` cwd with its
own `heso identity init` keypair (no shared state, no real network):

| Test | What it asserts |
|---|---|
| `round_trip_sign_then_verify_with_correct_allowlist_passes` | `heso open <wiremock> --receipt r.json` then `receipt-verify --trusted-keys allow.json r.json` → exit 0, stdout starts with `OK <signing-pubkey>` |
| `round_trip_verify_with_wrong_allowlist_is_rejected` | Same sign, verify with a different (bogus) pubkey in the allowlist → exit 1, stderr contains `INVALID` and `allowlist` |
| `round_trip_tampered_receipt_is_rejected` | Mutate `seed: 0 → 999` after signing, verify → exit 1, stderr `INVALID: signature verification failed` |
| `round_trip_mode_live_receipt_is_rejected` | `--receipt r.json --mode live`, verify → exit 1, stderr mentions `live` and `ADR 0008` |
| `round_trip_no_allowlist_warns_and_still_passes` | Verify with no `--trusted-keys` and `env_remove(HESO_TRUSTED_KEYS)` → exit 0 + stderr warning |
| `round_trip_env_allowlist_passes_with_correct_pubkey` | `env(HESO_TRUSTED_KEYS=allow.json)` instead of `--trusted-keys` → exit 0 |

Plus 13 unit tests on `receipts::tests` covering `url_trace`,
`try_consume_sign_flag` parsing happy-paths + rejections, allowlist
JSON parsing (array form, object `{"keys": [...]}` form, malformed
entry rejections), and `pubkey_in_allowlist` exact-match semantics.

Workspace `cargo test --workspace` was clean both before and after
the changes — 57 test sections, 0 failures.

## README diff

The new "Signed receipts" section was inserted between the
determinism example and "Error handling" in `README.md`. It walks
through:

1. `heso identity init` (one-time setup, with the exact JSON the
   verb returns).
2. `heso open <url> --receipt receipt.json` (with a copy-pasteable
   sample of the receipt JSON shape).
3. `heso receipt-verify --trusted-keys trusted.json receipt.json`
   (binding to a trusted signer + happy-path output).
4. The three rejection modes the verify side guarantees: tampered
   bytes, wrong signer (allowlist), `mode: live` (ADR 0008
   replay-safety).
5. `HESO_TRUSTED_KEYS` env-var alternative.
6. No-allowlist legacy behavior (still works, but warns).
7. Exit-code semantics (`0` valid + allowlisted, `1` invalid /
   tampered / wrong-signer / live-mode, `2` missing/malformed
   receipt or bad `--trusted-keys` source).

Full text is at `README.md` between the `## Signed receipts` heading
(new) and the existing `## Error handling` heading. Commit
`49436df`.

## Constraints satisfied

- [x] **End-to-end round-trip test (NEW)** — `tests/receipts_round_trip.rs`,
      6 tests, all passing. Sign via wiremock fixture → capture
      receipt → verify with correct allowlist passes → verify with
      different allowlist rejected → tamper rejected → `mode: live`
      rejected. Exercises the full CLI binary; no library-only
      mocks.
- [x] **README "Signed receipts" section** — 69 lines of copy-pasteable
      commands and JSON shape, lands between determinism example
      and error handling.
- [x] **`heso` no-args help lists the new verb / flag** — `--receipt`,
      `--key`, `--mode`, `--seed` on `heso open` and `heso read`;
      `--trusted-keys` on `heso receipt-verify`, plus the three new
      stderr-warning / rejection notes.
- [x] **`cargo test --workspace` clean** — 57 test result sections, 0
      failures (both with default features and after these changes).
- [x] **Granular commits — one per bug (3 minimum)** — 5 commits:
      `970d965` P0 + `36b2101` P1-allowlist + `c5aabf9` P1-mode-live
      + `34bacae` round-trip test + `49436df` README.
- [x] **Context7 / docs.rs for crate APIs** — I deliberately did NOT
      reach for raw `ed25519-dalek` / `blake3` / `base64` APIs.
      Everything goes through the existing `heso_core::IdentityKey`
      (which already wraps `ed25519-dalek` 2.2 correctly) and
      `heso_trace::{sign_receipt, verify_receipt, canonical_receipt_json}`.
      The CLI module is thin glue on top of those — no new crypto
      surface.
- [x] **Don't refactor the receipts crate structure** — `heso-trace`
      and `heso-trace-exec` are unchanged. All changes are in
      `heso-cli/src/` (one new file, one modified file) and in the
      new integration test file.
- [x] **Release binary built** — `cargo build --release -p heso-cli`
      green at the end. Demo flow above executed against
      `target/release/heso.exe`.

## What I deliberately did NOT do

- **Did not refuse-to-sign `mode: live` on the producing side.** The
  audit also flags this (P1 second half — `sign_receipt` should
  refuse `Mode::Live` per ADR 0005's comment). That's a
  library-level change in `heso-trace::sign_receipt`; the task
  scope was the CLI wiring + the verify-side rejection, so I left
  the sign-side guard for a separate change. The verify-side
  rejection in this commit makes a live-mode receipt useless even
  if it gets produced.
- **Did not add `--receipt` to `heso batch`.** Batch has N URLs per
  call; a single `--receipt PATH` would be ambiguous. Options
  include `--receipt-dir <dir>` (one file per URL) or a JSON-Lines
  receipts stream, but each has its own design surface. Out of
  scope for "wire the missing producer."
- **Did not touch ADR 0005 doc drift.** The audit flags ADR 0005
  referencing paths and crate names that don't exist
  (`~/.heso/identity/<agent-id>/key.priv`, `heso-identity` crate,
  `heso-audit` crate). That's a doc fix, separate from the wiring
  scope.
- **Did not implement the cassette / recording mode (ADR 0008).**
  Listed P1 in the audit as a separate fix; out of scope for "wire
  the producer."

## Summary

`--receipt PATH` on `heso open` / `heso read` produces signed
Receipts that verify against an optional `--trusted-keys` allowlist
(file or `HESO_TRUSTED_KEYS` env), with explicit rejection of both
tampered receipts (existing) and `mode: live` receipts (new). The
signed-receipts pitch is now reachable from the CLI, demonstrated
end-to-end against a hermetic wiremock fixture, documented in the
README, and exit-code-tested in CI.
