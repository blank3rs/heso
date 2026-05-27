# HESO/1.0

**Status:** 1.0 (provisional)
**Date:** 2026-05-26
**Editor:** Akshay (blank3rs)
**Reference implementation:** [`heso`](https://github.com/blank3rs/heso) v0.1.2
**License:** CC0 1.0 (spec text) · MIT or Apache-2.0 (reference implementation)

---

## Abstract

HESO is a wire-format protocol for **agent-driven web observation**. It defines four interlocking data structures — the **plat** (a content-addressed JSON observation of one web resource), the **cassette** (the embedded record of every HTTP exchange the observation touched), the **receipt** (a signed attestation of an executed action trace), and the **verb namespace** (the canonical names agents use to act on the web) — plus the **determinism rules** that let any conformant implementation re-execute a plan and produce a byte-identical hash.

The protocol's design center is reproducibility-backed trust. A plat + a signing key + any conformant implementation is sufficient to (a) verify the observation was not tampered with, (b) re-execute the captured plan off-network and produce the same hash, and (c) reason about what an agent saw without re-fetching anything.

HESO is implementation-agnostic. The reference implementation is one Rust binary; this specification is what makes a second implementation in any language possible.

## Status of this specification

HESO/1.0 is **provisional**. The wire formats in §1–§5 are committed: any change is a HESO/1.x (additive) or HESO/2.0 (breaking) versioning event. §1.9 carries six pinned canonicalization vectors that any implementation MUST round-trip; equivalent vectors for §3 (receipts with reproducible signatures, requires a fixed test keypair) and §5.3 (entropy surfaces) are scheduled for the next revision.

A specification is fully validated only after an independent second implementation passes the §6 conformance suite. Until then, "spec by reference implementation" is the operational truth in places §6 has not yet covered; those gaps are named in Appendix A.

## Conformance terminology

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**, **SHOULD**, **SHOULD NOT**, **RECOMMENDED**, **MAY**, and **OPTIONAL** in this document are to be interpreted as described in [RFC 2119](https://datatracker.ietf.org/doc/html/rfc2119) and [RFC 8174](https://datatracker.ietf.org/doc/html/rfc8174) when, and only when, they appear in all capitals.

## Document conventions

- Hexadecimal literals are lowercase unless otherwise noted.
- JSON examples are presented in compact form for byte-level fidelity. Implementations MAY emit pretty-printed JSON; the §1.5 canonicalization rules govern hash inputs.
- The notation `BLAKE3(x)` refers to the BLAKE3 hash function with default 32-byte (256-bit) output. The notation `lowercase_hex(d)` denotes the 64-character lowercase hexadecimal encoding of digest `d`.
- The notation `crates/<name>/src/...` refers to reference implementation source paths in the heso repository for traceability; conformance does not require the same file layout.

---

## §1 Plat Format

### §1.1 Scope

A **plat** is an immutable, content-addressed JSON object that records a single point-in-time observation of a web resource. Every HESO/1.0-conformant implementation MUST be able to produce and consume plats per this section.

The plat is the protocol's primitive unit of trust: holding a plat and a conformant implementation is sufficient to (a) recompute its content hash, (b) verify the embedded `plat_hash` matches, and (c) reason about what an agent observed without re-fetching the network.

### §1.2 Container and encoding

A plat MUST be a JSON object as defined in [RFC 8259](https://datatracker.ietf.org/doc/html/rfc8259), encoded as UTF-8.

- A plat file SHOULD use the file extension `.plat`. Implementations MUST NOT depend on the extension for type detection — the canonical type is the JSON content.
- Implementations MUST accept input with or without a trailing newline.
- Implementations MUST NOT emit a UTF-8 byte-order mark.
- The `Content-Type` for HTTP transport SHOULD be `application/vnd.heso.plat+json`. Implementations MUST accept `application/json` for backwards compatibility with generic JSON tooling.

### §1.3 Top-level fields

A plat is a JSON object. The following table defines the canonical top-level fields. Implementations MAY add fields not listed here; the canonicalization rules in §1.5 control which fields contribute to `plat_hash`.

**Fields required in every plat:**

| Field | Type | Description |
|---|---|---|
| `input_url` | string | The verbatim URL string the caller supplied, before any parsing or normalization. Distinct from `url` to preserve byte-level distinctions URL parsing would collapse (case, trailing slashes, default ports). |
| `url` | string | The parsed, post-redirect final URL, as produced by an RFC 3986 URL parser. |
| `title` | string | The page title. MAY be the empty string. |
| `description` | string | The page meta description. MAY be the empty string. |
| `tree` | object | The navigable section tree derived from heading structure. Internal schema deferred to §1.10. |
| `actions` | array | Interactive element references with stable `@eN` refs. Internal schema deferred to §1.11. |
| `plat_hash` | string | The 64-character lowercase hexadecimal BLAKE3 digest defined in §1.6. |

**Fields required in plats produced by the `stamp` and `run` verbs (see §4):**

| Field | Type | Description |
|---|---|---|
| `plan` | array of Action | The action sequence the implementation executed. See §1.4. |
| `cassette` | object | The recorded network observations. See §2. The cassette field IS hashed; tampering with it changes `plat_hash`. |
| `steps` | array of Step | The per-action execution log produced by stamping or running the plan. One Step per executed Action, in execution order. See §1.4.1. The `steps` field IS hashed; per-step `status` and `observed` payloads contribute to `plat_hash`. |

**Verb-dependent or optional fields:**

| Field | Type | When present |
|---|---|---|
| `text` | string | Visible body text after JS execution. Emitted by `heso open --inject-script` and by `heso read`. |
| `console` | array of string | JavaScript console buffer. |
| `inline_data` | object | Server-side hydration payloads (for example `__NEXT_DATA__`). |
| `data_attrs` | object | JSON values extracted from HTML `data-*` attributes. |
| `linked_pages` | array of plat | Pre-fetched same-origin links, each itself a plat. Forms a Merkle tree (parent commits to each child's `plat_hash`). |
| `forms`, `cookies`, `framework`, `scripts` | various | Extension fields requested via `--include`. |
| `http_status` | number | HTTP status code of the final response. |
| `partial` | boolean | `true` if the page is degraded. |
| `partial_reason` | string | Token describing degradation (`script_crash`, `fetch_failed`, `bot_challenge`, `http_403`, `http_5xx`, ...). |
| `failed_scripts` | array | Script-execution failure records. |
| `console_errors_count` | number | Count of `console.error` calls. |

### §1.4 Action object

Each entry in `plan` is an Action — a JSON object internally tagged by a `verb` field. HESO/1.0 §1 defines four plan-resident action verbs; additional verbs at the protocol level (tools, not plan entries) are defined in §4.

| Verb | Required fields | Wire example |
|---|---|---|
| `open` | `url` (string) | `{"verb":"open","url":"https://example.com/"}` |
| `click` | `ref` (string, matching `@eN` / `@formN` / `@aN`) | `{"verb":"click","ref":"@e3"}` |
| `fill` | `ref` (string), `value` (string) | `{"verb":"fill","ref":"@e7","value":"hello"}` |
| `submit` | `ref` (string) | `{"verb":"submit","ref":"@form1"}` |

Implementations MUST reject Action objects with an unknown `verb` value per §4.4. Implementations MUST NOT silently ignore unknown fields within a known verb. Implementations MUST preserve the relative ordering of Action objects in `plan`.

### §1.4.1 Step object

Each entry in `steps` is a Step — a JSON object recording one executed Action and the outcome the implementation observed. The `steps` array MUST contain exactly one Step per Action the implementation attempted, in execution order, indexed contiguously from `0`.

| Field | Type | Description |
|---|---|---|
| `index` | number | Zero-based position of the step within `steps`. MUST equal the array index. |
| `verb` | string | The Action's `verb` field, repeated for ergonomics (lets a consumer scan `steps[].verb` without dereferencing `steps[].action.verb`). |
| `action` | Action | The Action object exactly as it appears at `plan[index]`, byte-identical. |
| `url_before` | string | The URL the engine was on when the step began. For step `0`, this is the plan's entry URL (the first `open` Action's `url`). |
| `url_after` | string | The URL the engine is on when the step ends. For navigating verbs (`open`, `click` on an unhandled `<a href>`, `submit` that POSTs) this is the post-redirect final URL; otherwise it equals `url_before`. |
| `status` | string | Three-way outcome (§1.4.1.1). Exactly one of `"ok"`, `"partial"`, `"error"`. |
| `observed` | object | Verb-specific structured result. Present iff `status` is `"ok"` or `"partial"`; OMITTED when `status` is `"error"`. The shape is verb-dependent; the canonical form mirrors what the corresponding live verb (e.g. `heso open`) would emit on stdout. |
| `partial_reason` | string | Token explaining a `partial` outcome (§1.4.1.2). REQUIRED when `status` is `"partial"`; absent otherwise. |
| `error` | string | Human-readable failure message. REQUIRED when `status` is `"error"`; absent otherwise. |
| `started_at` | string | Deterministic logical RFC 3339 timestamp at the step's beginning (§1.4.1.3). |
| `finished_at` | string | Deterministic logical RFC 3339 timestamp at the step's end (§1.4.1.3). |

Implementations MUST NOT add wall-clock-derived fields to a Step. Recording the host clock in any byte that contributes to `plat_hash` would violate the §1.7 Property 1 determinism contract; the `started_at` / `finished_at` construction in §1.4.1.3 is the spec-mandated alternative.

When the plan halts on an `"error"` step, `steps` MUST contain entries for every Action up to and including the failing one. Actions after the failing one MUST NOT appear in `steps`.

#### §1.4.1.1 Step `status` values

| Value | Meaning |
|---|---|
| `"ok"` | The verb's contract was met end-to-end. For `open`: a 2xx response that is not a bot-challenge body. For `click` / `fill` / `submit`: the live DOM matched the target selector and the dispatched event was not rejected by the verb's own semantics. |
| `"partial"` | The verb ran but produced a degraded outcome. The plan was not aborted; execution continues with the next step. A `partial_reason` token MUST accompany the step. |
| `"error"` | The verb could not run. Examples: an `open` whose underlying network call failed, a `click` whose `ref` could not be resolved at all, a malformed Action. The plan MUST halt at this step. An `error` field MUST accompany the step. |

#### §1.4.1.2 Step `partial_reason` tokens

When `status` is `"partial"`, the `partial_reason` field carries one of the tokens below. The tokens are the same vocabulary used by the top-level plat `partial_reason` field (§1.3) — implementations SHOULD reuse them so consumers can apply the same handlers at both granularities.

| Token | Trigger |
|---|---|
| `http_NNN` | HTTP response status in 3xx, 4xx, 5xx, or 1xx (literal status substituted, e.g. `http_404`). |
| `http_5xx` | HTTP response in the 5xx range, when the implementation chooses the bucketed form over `http_NNN`. |
| `bot_challenge` | A 2xx response whose body is recognised as a Cloudflare / generic anti-bot interstitial. |
| `selector_not_matched` | A `click` / `fill` / `submit` whose Action ref resolved at the snapshot level but did not match in the live DOM (DOM mutation between snapshot and dispatch). |
| `script_crash` | Page-owned JavaScript threw during the verb's execution. |
| `fetch_failed` | A subresource fetch the verb triggered failed in a way that degraded the result without aborting it. |

Implementations MAY define additional tokens for their own reference-implementation needs; spec-defined tokens MUST be used in preference when applicable.

#### §1.4.1.3 Deterministic logical timestamps

The `started_at` and `finished_at` fields are RFC 3339 UTC strings with millisecond precision (`YYYY-MM-DDTHH:MM:SS.sssZ`). Their values MUST be derived from the step `index` alone — never from the host wall clock, the OS monotonic clock, or any source that varies across hosts.

Given a step index `i` (zero-based):

```
ms_started   := i * 2
ms_finished  := i * 2 + 1
started_at   := rfc3339_utc_ms(ms_started)    // ms offset from 1970-01-01T00:00:00.000Z
finished_at  := rfc3339_utc_ms(ms_finished)
```

The construction yields a strictly monotonic, fully deterministic sequence in which each step occupies a 1 ms slice and adjacent steps do not share boundaries. Stamping and replay produce the same string for the same step index — the determinism contract in §1.7 Property 1 holds.

The choice of synthetic epoch (`1970-01-01T00:00:00.000Z`) matches the virtual-clock origin in §5.4.1, so a consumer scanning `steps[].started_at` and the JS-side `Date.now()` traces sees one consistent zero point. Implementations MUST NOT emit a non-zero epoch offset for the `steps` timestamps in HESO/1.0; a future minor version MAY add an opt-in `epoch_offset_ms` field per the open-question note in §5.4.1.

#### §1.4.1.4 Per-step replay assertion

The `run` verb (§4.7) MUST, when the input plat carries a `steps` array, walk that array in execution order and compare each recorded Step against the corresponding re-executed Step:

1. If the recorded and re-executed `steps` arrays differ in length, the implementation MUST report a length mismatch and surface it to the operator (stderr, structured error, or both).
2. For each index `i` present in both arrays, the implementation MUST compare:
   - The recorded `status[i]` against the re-executed `status[i]` (string equality).
   - The recorded `observed[i]` against the re-executed `observed[i]` (JSON value equality).
3. Any mismatch MUST be surfaced with at least the diverging step index and the name of the diverging field (`status` or `observed`).
4. When at least one per-step mismatch is detected, the implementation MUST exit non-zero (per the §4.7 exit-code taxonomy, this is operational failure `1`).

The per-step assertion is a defense-in-depth surface over the §1.6 plat-hash check: a cassette mutation that flips a step's observable behaviour will already change `plat_hash`, but the per-step diff localises the divergence to a specific Step and field, which is the diagnostic information the operator needs to triage a drifted page.

### §1.5 Canonical bytes

To derive the canonical bytes of a plat `P`:

1. **Strip the top-level `plat_hash` field** of `P` if present. A hash field cannot contain its own digest. Nested `plat_hash` keys (for example inside `linked_pages[*]`) MUST be preserved — these form the Merkle commitment from a parent plat to its children.

2. **Preserve every other field exactly as emitted.** Cookies, console output, inline hydration data, metadata, DOM attributes, partial flags, HTTP status, and action attrs all contribute to `plat_hash` when present. If an implementation emits a byte into the plat body, the hash commits to it.

3. **Apply [RFC 8785](https://datatracker.ietf.org/doc/html/rfc8785)** (JSON Canonicalization Scheme) to the value from steps 1 and 2. Specifically:
   - Object keys MUST be sorted by UTF-16 code-unit order, per RFC 8785.
   - Numbers MUST be serialized per ECMA-262 `Number.prototype.toString`.
   - Strings MUST use JCS string escapes.
   - Unicode MUST NOT be normalized — NFC and NFD codepoint sequences hash distinctly. This is a deliberate property; an attacker cannot smuggle equivalent text through a normalization step.

The output is a sequence of UTF-8 bytes, denoted `canonical_bytes(P)`.

### §1.6 plat_hash construction

The `plat_hash` field MUST be computed as:

```
plat_hash := lowercase_hex(BLAKE3(canonical_bytes(P)))
```

- Hash algorithm: **BLAKE3** with default 32-byte (256-bit) output.
- Encoding: 64 ASCII characters, lowercase hexadecimal `[0-9a-f]+`.
- No prefix. The spec reserves the option to add a hash-agility prefix in a future major version; HESO/1.0 implementations MUST emit and accept the bare hex.
- The result MUST be embedded into `P` under the top-level key `plat_hash` as a JSON string.

A verifier MUST:

- Recompute `lowercase_hex(BLAKE3(canonical_bytes(P)))`.
- Compare against the embedded `plat_hash` string.
- Treat a mismatch as a tamper signal (return value of the verify operation), not an exception or error.
- Treat a missing or non-string `plat_hash` as a malformed-plat error, distinct from a tamper signal.

### §1.7 Properties

The §1.5 / §1.6 construction guarantees, normatively:

1. **Determinism.** For any plat `P`, every conformant implementation MUST produce identical `canonical_bytes(P)` and therefore identical `plat_hash(P)`. Cross-implementation byte-identical replay reduces to this property.
2. **Distinctness on emitted-content changes.** Two plats whose emitted body differs by any byte other than the top-level `plat_hash` MUST produce different `plat_hash` values.
3. **Redaction changes identity.** Removing any present content field before sharing a plat MUST produce a different `plat_hash` and invalidate any previous signature. The top-level `plat_hash` field itself is bookkeeping and remains excluded from its own digest.
4. **Merkle commitment to children.** A plat with `linked_pages[i].plat_hash = h_i` MUST have its own `plat_hash` change if any `h_i` changes. The nested `plat_hash` keys are ordinary content during canonicalization.
5. **Empty-vs-null-vs-absent distinguishability.** A field present with value `""`, value `null`, and a field absent entirely MUST produce three distinct hashes. An attacker MUST NOT be able to elide a field without changing `plat_hash`.
6. **Unicode form sensitivity.** NFC and NFD codepoint sequences for the same grapheme produce distinct `plat_hash` values. Implementations MUST NOT silently normalize.

### §1.8 Sealed envelope (signed plat)

For attestation use cases — proving "implementation X with key K produced this plat" — a plat MAY be wrapped in a sealed envelope:

```json
{
  "alg": "heso-plat/v1+ed25519",
  "content": { "...plat body, including its own plat_hash..." },
  "signature": {
    "algorithm": "Ed25519",
    "public_key": "<base64>",
    "signature": "<base64>"
  }
}
```

**Construction:**

1. Compute `plat_hash` and embed it in `content` per §1.6.
2. Construct the signing payload as a 13-byte domain-separation prefix followed by the canonical bytes of `content`:
   ```
   payload := SIGNING_DOMAIN || canonical_bytes(content)
   SIGNING_DOMAIN := the byte sequence "heso-plat/v1\x00"   (12 ASCII chars + 1 NUL)
   ```
3. Sign `payload` with the implementation's Ed25519 secret key per [RFC 8032](https://datatracker.ietf.org/doc/html/rfc8032). The output goes in `signature.signature`.

**Verification** MUST follow this order, exiting on the first failure:

1. Reject if `alg != "heso-plat/v1+ed25519"`. Verifiers MUST refuse unknown algorithms rather than fall back.
2. Recompute `BLAKE3(canonical_bytes(content))` and compare against `content.plat_hash`. Mismatch is a "hash mismatch" error distinct from a signature failure. The signature check MUST NOT be performed on a hash-mismatched envelope (the body has been mutated; reporting a signature failure would be misleading).
3. Verify the Ed25519 signature over `SIGNING_DOMAIN || canonical_bytes(content)` using the embedded `public_key`. Implementations MUST use `verify_strict` (per [RFC 8032 §8.4](https://datatracker.ietf.org/doc/html/rfc8032#section-8.4)) to reject signature malleability.

If all three pass, the envelope is **Valid**.

### §1.9 Test vectors

The following vectors are the conformance set for §1.5 canonicalization and §1.6 hash construction. Every HESO/1.0-conformant implementation MUST reproduce these exact `plat_hash` values when fed the corresponding input JSON.

> **Note on ordering.** Object keys in the *input JSON* below appear alphabetically because that is what `serde_json::Value` emits. Implementations MAY feed input JSON with keys in any order; only the canonical form per §1.5 determines the hash.

#### V1 — Minimal plat

Input JSON (also canonical bytes):
```
{"actions":[],"description":"","input_url":"https://example.com/","title":"Example","tree":[],"url":"https://example.com/"}
```
`plat_hash`:
```
bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf
```

#### V2 — Merkle parent over two child plat_hashes

Demonstrates §1.7 Property 4. Changing either `aaaa` or `bbbb` flips the parent `plat_hash`.

Input JSON (also canonical bytes):
```
{"actions":[],"description":"","input_url":"https://example.com/","linked_pages":[{"plat_hash":"aaaa","url":"https://example.com/a"},{"plat_hash":"bbbb","url":"https://example.com/b"}],"title":"Parent","tree":[],"url":"https://example.com/"}
```
`plat_hash`:
```
f098b1ac08693b85c05fc9465a9f7763d22fb8563e292b025f7dbab9cc67ac62
```

#### V3 — V1 plus populated telemetry fields

Demonstrates §1.7 Property 3 — emitted telemetry fields contribute to the hash.

Input JSON (also canonical bytes):
```
{"actions":[],"console":["log"],"cookies":[{"name":"s","value":"session-123"}],"description":"","http_status":200,"id":"page-uuid-7f3a2","input_url":"https://example.com/","partial":false,"partial_reason":"ok","title":"Example","tree":[],"url":"https://example.com/"}
```
`plat_hash`:
```
a6c4dcef1d2c5e96a6abb47878df0a905336f5a557f7a8b1d99f76da49c351b9
```

#### V4 — Unicode NFC (`é` as U+00E9, UTF-8 `0xC3 0xA9`)

Input JSON (also canonical bytes):
```
{"actions":[],"description":"","input_url":"https://example.com/café","title":"café","tree":[],"url":"https://example.com/café"}
```
`plat_hash`:
```
a64f1bf864d5eba5972a4a41fed19144077fedf23c9626c9b7adf57343b6c650
```

#### V5 — Unicode NFD (`é` as `e` + combining acute, U+0065 U+0301)

Same string as V4 to a human reader. Decomposed codepoint sequence. UTF-8 of `é` here: `0x65 0xCC 0x81`. Demonstrates §1.7 Property 6 — `plat_hash` differs from V4.

Input JSON (also canonical bytes — visually identical to V4, byte-different in the `é`):
```
{"actions":[],"description":"","input_url":"https://example.com/café","title":"café","tree":[],"url":"https://example.com/café"}
```
`plat_hash`:
```
0a514b8a155da02f7db89ae79fb9fa885cc7ba88bf6837f1139b4026abbe2f7d
```

#### V6a / V6b / V6c — Empty string vs null vs absent

Demonstrates §1.7 Property 5 — a `title` field present with `""`, present with `null`, and absent entirely MUST yield three distinct hashes.

**V6a (`title: ""`):**
```
{"actions":[],"description":"","input_url":"https://example.com/","title":"","tree":[],"url":"https://example.com/"}
```
`plat_hash`: `121f46f2d02fafadb811cd0ff2a1b7e5d6f64a381af29b36295384ba96f91c4b`

**V6b (`title: null`):**
```
{"actions":[],"description":"","input_url":"https://example.com/","title":null,"tree":[],"url":"https://example.com/"}
```
`plat_hash`: `801a174528591c1ef1cd3e3d249f76f277be8e84675b4758791b1e1355d2aa41`

**V6c (`title` absent):**
```
{"actions":[],"description":"","input_url":"https://example.com/","tree":[],"url":"https://example.com/"}
```
`plat_hash`: `e53bdc36b6aa0dbc27679d4c1a0dae825e9f500c48915357f9e34dfd49cb8c45`

#### V7 — Sealed envelope (TBD)

Reserved for the next revision. Shape and verification steps are specified in §1.8; concrete signature bytes require a fixed test keypair generated from a published seed.

#### V8 — Plat with a single `ok` step (§1.4.1)

Pins the canonical bytes of a stamped plat containing one `steps[]` entry — exercises every required Step field (`index`, `verb`, `action`, `url_before`, `url_after`, `status`, `observed`, `started_at`, `finished_at`) and the deterministic logical-timestamp construction from §1.4.1.3.

Input JSON (also canonical bytes):
```
{"actions":[],"description":"","input_url":"https://example.com/","plan":[{"url":"https://example.com/","verb":"open"}],"steps":[{"action":{"url":"https://example.com/","verb":"open"},"finished_at":"1970-01-01T00:00:00.001Z","index":0,"observed":{"http_status":200,"op":"open"},"started_at":"1970-01-01T00:00:00.000Z","status":"ok","url_after":"https://example.com/","url_before":"https://example.com/","verb":"open"}],"title":"Stepped","tree":[],"url":"https://example.com/"}
```
`plat_hash`:
```
f550be12cd6cff8d738d9f80947ecce676e375077c99e75a0bffc4ae8f847ad1
```

### §1.10, §1.11 Tree and actions internal schema

The `tree` and `actions` fields are REQUIRED (§1.3) but their internal schemas are not yet normalized for the spec. Reference implementation defines them in `crates/heso-engine-fetch/src/lib.rs`; spec language to follow once a second implementation has stress-tested whether the current shape is implementable without copying Rust struct definitions.

---

## §2 Cassette Wire Format

### §2.1 Scope

A **cassette** is the recorded set of HTTP exchanges that occurred during the production of a plat. Cassettes are written by `recording` and `live`-with-recording verbs and consumed by `deterministic` replays. They are the mechanism by which §5.5 (network input) provides byte-identical replay off-network.

A cassette MUST be embedded as the top-level `cassette` field of a plat. HESO/1.0 specifies a single wire shape; sidecar storage is out of scope and is deferred to a future major version.

### §2.2 Cassette object

The `cassette` field is a JSON object with a single field, `records`, an ordered array of Record objects:

```json
{
  "records": [
    { "...Record 1..." },
    { "...Record 2..." }
  ]
}
```

Implementations MUST preserve the relative ordering of `records` across encode/decode. Implementations MUST NOT add fields beside `records` at the cassette object level for HESO/1.0; a future version MAY introduce them additively.

### §2.3 Record object

Each Record is a JSON object with exactly these fields:

| Field | Type | Description |
|---|---|---|
| `method` | string | Uppercase HTTP method (`GET`, `POST`, `PUT`, `DELETE`, ...). |
| `url` | string | Request URL as the client emitted it (pre-redirect). |
| `final_url` | string | URL of the response after redirect resolution. Equal to `url` when no redirect occurred. |
| `request_body_b64` | string | Standard base64 (RFC 4648 §4) of the raw request body. Empty body is `""`, never `null`. |
| `status` | number | HTTP response status code (integer in [100, 599]). |
| `response_headers` | array | Ordered array of `[name, value]` two-element string arrays, in the order returned by the server. Duplicate header names MUST be preserved. |
| `response_body_b64` | string | Standard base64 of the raw response body bytes. Empty body is `""`. |
| `response_body_blake3` | string | 64-character lowercase hex BLAKE3 of the raw response body bytes (pre-base64-encoding). Required in HESO/1.0. Provides content addressing in place. |

Implementations MUST emit every field above; they MUST NOT add new fields to a Record for HESO/1.0.

### §2.4 Canonicalization

The cassette object participates in `plat_hash` per §1.5 — every byte in every `response_body_b64`, every `response_body_blake3`, and every `response_headers` entry contributes to the hash. There is no "reference-only" mode where bytes are elided from the canonical input.

The `response_body_blake3` field is REQUIRED redundancy. A verifier MUST treat a `response_body_blake3` that does not equal `lowercase_hex(BLAKE3(base64_decode(response_body_b64)))` as a malformed-cassette error.

### §2.5 Miss semantics

A **cassette miss** is a lookup whose `(method, url, request_body)` triple is not present in the cassette.

Implementations executing in `deterministic` mode (§5.2) MUST surface a miss as a structured, fatal error to the caller. The error MUST carry the requesting `method`, `url`, and a count of records currently in the cassette so a downstream operator can distinguish "cassette empty" from "cassette present but request drifted." The reference implementation's wire shape is:

```
cassette miss: METHOD URL not recorded (cassette has N records)
```

Implementations MUST NOT silently re-fetch on a miss under any circumstance. A silent re-fetch is a §5.5 conformance violation.

When the cassette contains multiple records whose `(method, url, request_body)` triples are equal, HESO/1.0 implementations MUST return the first matching record. Sequential-cursor replay is deferred to a future minor version (see Appendix A).

### §2.6 Size

HESO/1.0 does not normatively cap cassette size. Implementations MAY refuse plats above a configurable limit; the RECOMMENDED default refusal threshold for general-purpose consumers (registries, viewers) is 64 MB. A plat over that limit is not malformed — it is shippable between consenting implementations but SHOULD NOT be expected to round-trip through size-constrained channels.

---

## §3 Receipt Format

### §3.1 Scope

A **receipt** is an immutable JSON object that records a single execution of an action trace by a HESO/1.0-conformant implementation. Every conformant implementation MUST be able to produce and consume receipts per this section.

A receipt attests, in one signed envelope, to:

1. **What was planned.** The full ordered sequence of primitive operations the implementation set out to perform.
2. **What happened.** The per-op results, the index and error of the first failed op (if any), and any content hashes the engine reported for pages it touched.
3. **Under what rules.** The determinism mode, the session seed, and the planner identifier, exactly as the implementation honored them at run time.
4. **Who is claiming it.** The signer's Ed25519 public key, embedded in the signature envelope.

A receipt is distinct from a §1.8 sealed plat envelope. The §1.8 envelope signs canonical plat bytes; a §3 receipt signs a planned-trace-plus-outcome JSON object whose schema and canonicalization rules are defined here. Receipts and sealed plats MAY coexist in a single verb invocation; neither subsumes the other.

Receipts emitted in `mode: live` MUST NOT be considered verifiable evidence of a reproducible run; see §3.7.

### §3.2 Container and encoding

A receipt MUST be a JSON object as defined in RFC 8259, encoded as UTF-8.

- A receipt file SHOULD use the file extension `.receipt.json` or `.json`. Implementations MUST NOT depend on the extension for type detection.
- Implementations MAY emit a receipt as either compact JSON or pretty-printed JSON. On-wire formatting does not affect signature validity, because canonical bytes are recomputed from the parsed object on every verify (see §3.4).
- Implementations MUST NOT emit a UTF-8 byte-order mark.
- The `Content-Type` for HTTP transport SHOULD be `application/vnd.heso.receipt+json`. Implementations MUST accept `application/json` for backwards compatibility.

### §3.3 Top-level fields

A receipt is a JSON object. The following table defines the canonical top-level fields. Implementations MUST emit exactly the fields below — no more, no fewer — to remain byte-identical under §3.4.

**Fields always present:**

| Field | Type | Description |
|---|---|---|
| `trace` | array | The full planned action sequence — an ordered list of primitive-op objects. Schema defined in §4. Empty array is legal. |
| `results` | array | Per-op results, parallel to `trace`. If execution halted at a failed op, `results.len()` MUST equal `failed_at`; otherwise `results.len() == trace.len()`. |
| `trace_hash` | string | 64-character lowercase hex BLAKE3 digest of the canonical-JSON encoding of `trace`. See §3.5. |
| `seed` | number | Unsigned 64-bit integer session seed used for determinism shims (PRNG, virtual clock). Range `[0, 2^64 - 1]`. |
| `mode` | string | One of `"deterministic"`, `"recording"`, `"live"`. Lowercase only. See §3.7. |
| `cost` | object | Resource accounting; see §3.3.1. MUST always be present even if all fields are zero. |

**Fields conditionally present** (see §3.3.2 for the absent-vs-null rule):

| Field | Type | When present |
|---|---|---|
| `pages_seen` | array of string | Content hashes (64-char lowercase hex BLAKE3, per §1.6) of pages the engine reported during the run. OMITTED when the array is empty. |
| `planner_id` | string | Opaque planner-version tag. OMITTED when the string is empty. |
| `failed_at` | number | 0-based index into `trace` of the first op that failed. OMITTED when the run completed without failures. |
| `error` | string | Human-readable error message from the failed op. OMITTED when the run completed without failures. When `failed_at` is present, `error` MUST also be present. |
| `signature` | object | Ed25519 signature envelope; see §3.3.3. OMITTED when the receipt is unsigned. An unsigned receipt is legal as an intermediate artifact but MUST NOT be presented as attestation. |
| `tsa_anchor` | object | RFC 3161 trusted-timestamp anchor; see §3.3.4. OMITTED when the receipt has not been notarized. |

#### §3.3.1 The `cost` object

The `cost` object MUST contain exactly these four fields:

| Field | Type | Meaning |
|---|---|---|
| `bytes` | number | Bytes downloaded across the trace. Unsigned 64-bit integer. |
| `cpu_ms` | number | CPU time consumed, in milliseconds. Unsigned 64-bit integer. |
| `wall_ms` | number | Wall-clock time consumed (virtual clock in `deterministic` mode). Unsigned 64-bit integer. |
| `planner_tokens` | number | Planner tokens consumed by the layer that produced `trace`. Unsigned 64-bit integer. |

All four fields MUST always be emitted. Implementations that have not yet wired cost reporting MUST emit zeros; the field participates in the signature regardless.

#### §3.3.2 Absent vs null vs zero

For fields marked OMITTED above, implementations MUST omit the JSON key entirely when the value is absent. They MUST NOT serialize a JSON `null` placeholder. This mirrors §1.7 Property 5: present-with-default-value, present-with-`null`, and absent produce three different canonical byte strings and therefore three different signatures.

The always-present fields (`trace`, `results`, `trace_hash`, `seed`, `mode`, `cost`) MUST always appear, even when their value is the type's zero (empty array, zero seed, default `cost`).

#### §3.3.3 The `signature` envelope

When present, `signature` MUST be a JSON object with exactly three string fields:

| Field | Type | Description |
|---|---|---|
| `algorithm` | string | MUST be the literal `"Ed25519"` for HESO/1.0. Verifiers MUST reject any other value with an "unknown algorithm" error before attempting any cryptographic operation. |
| `public_key` | string | Standard base64 (RFC 4648 §4, with `=` padding) of the 32-byte Ed25519 verifying key. MUST decode to exactly 32 bytes. |
| `signature` | string | Standard base64 of the 64-byte Ed25519 signature. MUST decode to exactly 64 bytes. |

Implementations MUST use standard base64 (alphabet `A-Z a-z 0-9 + /`, padding `=`). Implementations MUST NOT use base64url (`-` / `_`).

#### §3.3.4 The `tsa_anchor` object

When present, `tsa_anchor` MUST be a JSON object with exactly these five string fields:

| Field | Type | Description |
|---|---|---|
| `tsa_url` | string | Absolute URL of the RFC 3161 Time-Stamp Authority that issued the token. Informational; verifiers MAY use it to fetch CRL/OCSP for revocation checking but MUST NOT depend on the URL being reachable at verify time. |
| `hash_alg` | string | Lowercase identifier of the digest algorithm used for the TSA `messageImprint`. MUST be one of `"sha256"`, `"sha384"`, `"sha512"`. Implementations MUST reject any other value when deserializing. |
| `message_imprint_hex` | string | Lowercase hexadecimal encoding of `HASH_ALG(canonical_receipt_bytes(R_pre))`, where `R_pre` is the receipt with BOTH `signature` and `tsa_anchor` cleared to JSON `null`, then canonicalized per §3.4. Length is 64 / 96 / 128 hex characters for `sha256` / `sha384` / `sha512` respectively. |
| `token_b64` | string | Standard base64 (RFC 4648 §4, with `=` padding) of the DER-encoded RFC 3161 `TimeStampToken` returned by the TSA. The token is a CMS `ContentInfo` wrapping a `SignedData` (RFC 3161 §2.4.2). The full token bytes are embedded so a receipt is self-contained for offline verification. |
| `gen_time` | string | ISO 8601 UTC timestamp extracted from the `TSTInfo.genTime` field inside the token. Informational and human-readable; the authoritative time is the field embedded inside the signed token bytes. |

The `tsa_anchor` field participates in `canonical_receipt_bytes(R)` per §3.4 when present. The §3.6 signing payload therefore covers `tsa_anchor` whenever the field is present at signing time. A receipt produced by `notarize` (§4.7) has its Ed25519 signature re-issued after the anchor is attached, so the signature covers the anchor; the TSA's internal `messageImprint`, by contrast, covers the *pre-anchor* canonical bytes (R with both `signature` and `tsa_anchor` cleared). The two cryptographic proofs therefore commit to overlapping but not identical byte strings — this is intentional and load-bearing: the Ed25519 signature attests to the complete receipt including the anchor, while the TSA token attests to the bytes that existed before the anchor was attached.

Implementations MUST emit standard base64 for `token_b64`. Implementations MUST NOT use base64url.

### §3.4 Canonical bytes

To derive the canonical bytes of a receipt `R`:

1. **Force `signature` to JSON `null`** on the top-level object. This produces a single canonical "unsigned" shape regardless of whether the input was freshly built or already signed. The signature is computed over the receipt-without-signature, then written back into the same struct.
2. **Preserve every other field exactly as emitted** per §3.3, including the OMIT rules for conditionally present fields. Omission of a field MUST NOT be re-introduced as `null` during canonicalization.
3. **Apply canonical JSON serialization** with the following rules:
   - Object keys MUST be sorted by byte-wise ASCII order (recursively, depth-first).
   - Output MUST be compact: no insignificant whitespace, no trailing newline.
   - Strings MUST be JSON-escaped via the standard escape set (`\"`, `\\`, `\n`, `\r`, `\t`, `\b`, `\f`, and `\uXXXX` for control characters below `0x20`); Unicode codepoints `>= 0x20` MUST be emitted as their UTF-8 bytes (not escaped).
   - Numbers MUST be serialized with the integer-vs-float distinction preserved: integers as bare decimal digits, floats via ECMAScript's number-to-string rules.
   - Unicode MUST NOT be normalized.

The output is a sequence of UTF-8 bytes, denoted `canonical_receipt_bytes(R)`.

**Note on RFC 8785 conformance.** The reference implementation emits a subset of RFC 8785 (JCS) sufficient for the value shapes a receipt actually carries, but not audited against the full JCS conformance suite. Implementations using a full RFC 8785 library SHOULD verify their output against the §3.10 test vectors (forthcoming) before claiming HESO/1.0 conformance.

### §3.5 trace_hash construction

The `trace_hash` field MUST be computed as:

```
trace_hash := lowercase_hex(BLAKE3(serde_json_compact(trace)))
```

where `serde_json_compact(trace)` is the default `serde_json` compact serialization of the `trace` array (no pretty-printing, no key reordering — receipt traces are arrays of objects whose keys are sorted by their producer's serializer).

- Hash algorithm: BLAKE3, 32-byte output.
- Encoding: 64 ASCII characters, lowercase hexadecimal.
- No prefix.

The result MUST be embedded into `R` under the top-level key `trace_hash` as a JSON string. The `trace_hash` field is part of `R`'s canonical bytes (it is not excluded the way §1.6 excludes `plat_hash`), so a verifier that recomputes `trace_hash` and finds a mismatch MUST surface that as a malformed-receipt error distinct from a signature failure.

**Note on `trace` canonicalization.** Unlike `canonical_receipt_bytes`, the `trace_hash` input is NOT the §3.4 sort-keys-recursively canonical form; it is `serde_json`'s plain compact output. This is load-bearing for cross-implementation reproducibility: a second implementation that re-orders the keys inside a primitive-op object before serializing will compute a different `trace_hash`. Implementations MUST emit primitive-op objects with the field order defined by §4.

### §3.6 Signing payload

The signing payload MUST be exactly `canonical_receipt_bytes(R)` per §3.4, with `signature` cleared to `null`. There is **no** domain-separation prefix.

```
payload := canonical_receipt_bytes(R)
```

The implementation MUST sign `payload` with its Ed25519 secret key per RFC 8032. The output goes in `signature.signature`.

**Note on domain separation.** The §1.8 sealed-plat envelope prefixes its payload with a 13-byte domain separator; §3 receipts do not. This is a real asymmetry and a candidate for closure in a future revision (see Appendix A). Implementations MUST follow the no-prefix rule for HESO/1.0 receipts.

### §3.7 Mode rules

The `mode` field MUST be one of three lowercase strings: `"deterministic"`, `"recording"`, `"live"`. Implementations MUST reject any other value when deserializing a receipt.

Verifiers MUST apply these rules:

1. **`mode: live`** — Verifiers MUST reject the receipt outright, BEFORE the signature check. Live-mode runs use real clocks, real RNG, and a real network; the signature attests only to a historical run that nothing can reproduce. The signature MUST NOT be reported as valid for a `mode: live` receipt even if the cryptography would otherwise verify.
2. **`mode: recording`** — Verifiers MUST accept the receipt for signature-validity purposes (the cryptographic check applies normally). Verifiers SHOULD surface a warning that the recorded inputs are required to reproduce the run; a `recording` receipt is a witness that the run happened, not a proof that it is replayable today.
3. **`mode: deterministic`** — Verifiers MUST accept the receipt unconditionally for signature validity. This is the only mode whose receipts make the protocol's full reproducibility claim.

### §3.8 Verification order

A verifier MUST apply the following checks in order, exiting on the first failure with the indicated outcome:

1. **Parse.** If the file does not parse as a JSON object matching the §3.3 schema, return `MALFORMED`.
2. **Reject `mode: live`.** If `receipt.mode == "live"`, return `INVALID`. Do not proceed.
3. **Check `signature` is present.** If absent, return `MISSING` — an unsigned receipt is unverifiable by definition.
4. **Check `signature.algorithm`.** If not `"Ed25519"`, return `INVALID` with an "unknown algorithm" error message. Verifiers MUST NOT fall back to any other algorithm.
5. **Decode the envelope.** Decode `signature.public_key` as standard base64 (MUST be 32 bytes, MUST lie on the Ed25519 curve) and `signature.signature` as standard base64 (MUST be 64 bytes). Any failure returns `INVALID`.
6. **Recompute canonical bytes.** Compute `canonical_receipt_bytes(R)` per §3.4 (with `signature` cleared).
7. **Verify the signature.** Verify the Ed25519 signature over `canonical_receipt_bytes(R)` using the decoded public key. Implementations MUST use the strict verification variant (`verify_strict` per RFC 8032 §8.4).
8. **Apply the trusted-key allowlist** per §3.9. If an allowlist is configured and `signature.public_key` is not in it, return `INVALID`.
9. **Apply the `--require-tsa` policy** (verifier-side). If the verifier was invoked with a TSA-required policy (CLI flag, env, or library configuration) and `tsa_anchor` is absent from the receipt, return `INVALID` with a "TSA anchor required but absent" message. When the verifier is not configured to require a TSA anchor, skip this step regardless of whether `tsa_anchor` is present. The `--require-tsa` policy contributes nothing to the signed payload; it is a verifier-side policy layer.
10. **Verify the TSA token signature** (only when `tsa_anchor` is present). Decode `tsa_anchor.token_b64` as standard base64 to recover the DER-encoded `TimeStampToken` (a CMS `ContentInfo` per RFC 3161 §2.4.2). Verify the token's internal signature using the signing certificate embedded in the token's `SignedData.certificates` field. If a trusted-roots policy is configured (`--tsa-trusted-roots`, equivalent env, or library configuration), additionally verify that the signing certificate chains to one of the configured root CAs per RFC 5280. Any failure returns `INVALID`. When no trusted-roots policy is configured, chain validation MUST be SKIPPED — the token's signature over the imprint is still verified using the embedded certificate, but the certificate is not checked against any external trust store.
11. **Verify the imprint matches the pre-anchor canonical bytes** (only when `tsa_anchor` is present). Compute `R_pre` by cloning the receipt with both `signature` and `tsa_anchor` cleared to JSON `null`. Compute `expected_imprint = HASH_ALG(canonical_receipt_bytes(R_pre))` where `HASH_ALG` is identified by `tsa_anchor.hash_alg`. The 64/96/128-char lowercase hex of `expected_imprint` MUST equal `tsa_anchor.message_imprint_hex`. Additionally, the `messageImprint` field embedded inside the decoded `TSTInfo` MUST byte-equal `expected_imprint`. Either mismatch is a tamper signal and MUST return `INVALID` with a message that distinguishes the layer that failed (`message_imprint_hex mismatch` vs `TSTInfo messageImprint mismatch`).

If all checks pass, return `VALID`.

### §3.9 Trusted-key allowlist

A verifier MAY consult a trusted-key allowlist: a list of base64-encoded Ed25519 public keys that are considered authorized signers in the verifier's context.

1. **Source precedence.** A CLI flag MUST take precedence over an environment variable, which MUST take precedence over no allowlist.
2. **File format.** An allowlist file MUST be a JSON document of one of these two shapes:
   - A bare array of strings: `["base64Pubkey1==", "base64Pubkey2=="]`
   - An object with a `keys` field of the same shape: `{"keys": ["base64Pubkey1=="]}` — leaves room for future fields (labels, expiry) without breaking the array shape.
3. **Comparison.** Pubkey comparison MUST be exact-byte: case-sensitive, no normalization. Leading and trailing whitespace inside an entry MAY be trimmed at load time.
4. **Empty allowlist.** When no allowlist source is configured, the verifier MUST proceed without one but SHOULD emit a warning that the trust anchor is unset.
5. **Failed allowlist load.** When an allowlist source is configured but cannot be loaded (file missing, malformed JSON, non-string entry, empty string entry), the verifier MUST exit with `MALFORMED`. A bad allowlist MUST NOT degrade to "no allowlist."

The allowlist contributes nothing to the signed payload; it is a verifier-side policy layer.

### §3.10 Test vectors

The conformance set for §3.4 canonicalization, §3.5 `trace_hash`, and §3.6 signing is reserved for a future revision. Reproducible signature vectors require a fixed Ed25519 keypair generated from a published seed. The same reservation applies to §1.9 V7; the two are tracked together because they share the fixed-keypair infrastructure.

### §3.11 TSA test vectors

The conformance set for §3.3.4 `tsa_anchor` verification is reserved for a future revision. Reproducible TSA-anchored vectors require either (a) a mock TSA producing deterministic `TimeStampToken` bytes for a known input, or (b) a captured exchange against a real TSA pinned to a stored response. The reference implementation's wiremock-backed integration tests in `crates/heso-cli/tests/notarize_round_trip.rs` exercise the full notarize → verify flow ahead of these vectors landing.

When the §3.11 vectors land they will be numbered V1 (valid TSA anchor, sha256), V2 (valid TSA anchor, sha512), and V3 (tampered `token_b64` that MUST fail step 10 of §3.8). The vectors will share `heso-compat-tests` fixture infrastructure with §1.9 V7 and §3.10.

---

## §4 Verb Namespace

### §4.1 Scope

HESO/1.0 defines two tiers of verbs: a closed **core** tier enumerated in this section, and an open **extension** tier under reverse-DNS namespacing. Verbs appear in three places: as the `verb` field of plan-resident Action objects (§1.4), as the CLI surface a reference implementation exposes, and as references in `trace` arrays inside receipts (§3.3).

### §4.2 Two-tier namespace

**Core tier — a closed table in §4.7.** Core names are bare ASCII lowercase tokens matching the regex `[a-z][a-z0-9-]*`. Adding or removing a core verb is a spec-version event, gated by a new ADR in the reference implementation's repository. There is no separate registry server, no submission process, no review board — the spec document itself is the registry.

**Extension tier — reverse-DNS, no registry.** Anyone MAY define a verb under a domain they control. The wire syntax is one or more DNS labels in reverse order, joined by `.`, followed by `.` and a kebab-case verb name matching `[a-z][a-z0-9-]*`. The full name MUST contain at least one `.`; a name with no `.` is by definition a core verb and MUST appear in §4.7 or the implementation MUST reject it per §4.4.

The `ca.heso.x.*` prefix is reserved for experimental verbs published by the heso project that have not yet earned a core slot. Promotion from `ca.heso.x.foo` to bare `foo` requires an ADR and a minor-version bump.

**Wire shape on the `verb` field:**

```json
{"verb": "click", "ref": "@e3"}                         // core
{"verb": "com.example.scrape-pricing", "url": "..."}     // extension
{"verb": "ca.heso.x.warc-export", "path": "..."}         // heso experimental
```

### §4.3 Wire syntax

| Tier | Regex | Examples |
|---|---|---|
| Core | `^[a-z][a-z0-9-]*$` | `open`, `click`, `plat-hash` |
| Extension | `^([a-z][a-z0-9-]*)(\.[a-z][a-z0-9-]*){2,}$` | `com.example.scrape-pricing`, `org.archive.warc-import` |

Verb names are case-sensitive. The canonical form is lowercase. Implementations MUST NOT case-fold during comparison.

### §4.4 Required dispatch behavior

1. An implementation MUST accept every core verb listed in §4.7 of the spec version it claims to support.
2. An implementation MAY implement any extension verb. Implementations MUST NOT silently ignore an unknown verb in a `plan` array; they MUST reject the plat with a structured `unknown verb: NAME` error, exit non-zero, and refuse to produce a plat from a partial execution. This matches the miss-semantics rule for cassettes in §2.5.
3. An implementation MUST NOT register its own non-domain name in the core tier. A bare token that is not in §4.7 is always an error, never a vendor extension.
4. Verb names are case-sensitive (per §4.3).
5. **Dispatch is local-only.** Verb-name resolution MUST be performed against an implementation's locally registered verb table. Implementations MUST NOT fetch verb implementations over the network in response to encountering a verb in a plan, receipt, or CLI invocation. Discovering a verb (a human reading a doc) and dispatching it (an implementation running the code) are distinct operations; HESO/1.0 places dispatch entirely on the implementation side. Receiving a plat with an unknown extension verb MUST be a non-network error.

### §4.5 Process for adding verbs

**Extension verbs.** None, by design. Publish a doc under your domain describing the input/output shape and the determinism mode the verb requires; the URL is discoverable from the reverse-DNS prefix. The spec does not require this doc to live at a particular path or be machine-readable; HESO/1.0 deliberately ships without a discovery protocol.

**Core verbs.** Write an ADR in the reference implementation repository, accept it, ship the verb in the reference implementation, bump the spec to HESO/1.x, update §4.7's table. Same workflow that produced the existing catalog.

### §4.6 Squatting and provenance

Reverse-DNS namespacing prevents *impersonation*: only the owner of `example.com` can publish a verb under `com.example.*`. It does NOT prevent *typosquatting* — `com.exarnple.foo` (a Latin homoglyph) and `com.example.foo` are distinct verb names that look identical to a human reader.

HESO/1.0 anchors trust on signing keys, not on verb names. A plat that uses `com.example.scrape-pricing` and is signed by `example.com`'s Ed25519 identity (§1.8) binds the verb to the publisher far more strongly than verb-name typing would. Receivers SHOULD pin to trusted signers via the §3.9 allowlist mechanism rather than rely on verb-name fidelity.

### §4.7 Core verb catalog (HESO/1.0)

HESO/1.0 defines 17 core verbs. They divide into **action verbs** (appear in `plan` arrays per §1.4 and are executed by stamping or running a plat) and **tool verbs** (CLI / programmatic operations on plats, cassettes, receipts, and identity).

#### Action verbs (4)

These are the verbs that MAY appear as Action objects inside a `plan` array. Their full wire shape is defined in §1.4.

| Verb | Required fields | Notes |
|---|---|---|
| `open` | `url` | Navigate to a URL and produce a page observation. |
| `click` | `ref` | Dispatch a click event on the element matching `ref`. |
| `fill` | `ref`, `value` | Set the value of the input matching `ref` and fire `input` + `change`. |
| `submit` | `ref` | Submit the form matching `ref`, serialize per `enctype`, POST, and observe the response. |

#### Tool verbs (13)

These are HESO/1.0 operations that act on plats, receipts, keys, and conditions. They are not plan-resident; they appear as CLI / programmatic surface.

For each tool verb the spec pins: command-line surface, input shape, top-level output fields, and exit-code semantics. Detailed nested shapes (the internal structure of `metadata`, `actions`, `forms`, etc.) reference §1.10 / §1.11 deferral and the reference implementation source.

| Verb | One-line role |
|---|---|
| `read` | Fetch + execute JS + return rich content (text, forms, cookies, console, scripts, framework, deltas). |
| `wait` | Block until a condition is true on a fetched page (`--selector-exists`, `--text-contains`, `--url-matches`, `--network-idle`, `--time`). |
| `stamp` | Execute a plan against the live web; mint a plat with embedded cassette and per-step `steps` log (§1.4.1). Exit non-zero if any step's `status` is `"error"`. |
| `run` | Re-execute a plan against the embedded cassette in the input plat. MUST NOT fall back to live HTTP when the cassette is missing (§5.5). When the input plat carries a `steps` array, MUST perform the per-step replay assertion in §1.4.1.4 and exit non-zero on any mismatch. |
| `replay` | Pure observation: extract and emit the `steps` field of a plat (§1.4.1). No execution, no network. |
| `unpack` | Extract and emit the `plan` field of a plat for standalone editing. |
| `identity-init` | Generate an Ed25519 keypair for signing. CLI surface MAY also expose `identity init` as a two-token subcommand. |
| `notarize` | Attach an RFC 3161 trusted-timestamp anchor (§3.3.4) to an existing signed receipt by sending `HASH_ALG(canonical_receipt_bytes(R_pre))` to a Time-Stamp Authority and embedding the returned token. Re-signs the receipt with the same Ed25519 key so the signature continues to cover the full body. Refuses unsigned receipts and `mode: live` receipts. |
| `receipt-verify` | Verify a receipt envelope per §3.8, applying the §3.9 allowlist and the optional §3.8 step 9–11 TSA checks. |
| `plat-hash` | Recompute and print `lowercase_hex(BLAKE3(canonical_bytes(P)))` for a plat file. Output is plain text (one line). |
| `plat-verify` | Recompute and compare against the embedded `plat_hash`. Output is plain text (one line). |
| `plat-seal` | Wrap a plat in a §1.8 Ed25519 envelope. Refuses already-sealed input. |
| `plat-unseal` | Verify a §1.8 envelope per §1.8. With `--extract`, emit the inner plat body for piping. |

**Output format.** Action verbs and rich tool verbs (`read`, `stamp`, `run`, `replay`, `unpack`, `identity-init`, `notarize`, `plat-seal`, `plat-unseal`) emit JSON on stdout. `notarize` emits the updated receipt object — i.e. the input receipt with the new `tsa_anchor` field added and the `signature` field re-issued over the new canonical bytes. The utility verbs `plat-hash`, `plat-verify`, and `receipt-verify` emit a single plain-text line for pipe-friendliness (`blake3:<hex>`, `OK blake3:<hex>`, `OK <pubkey>`). Implementations MAY offer a `--json` flag to switch utility verbs to JSON output; HESO/1.0 does not require it.

**Exit codes.** Across all verbs:
- `0` — success
- `1` — operational failure (signature mismatch, hash mismatch, step failed, fetch failed, condition timeout without `--best-effort`)
- `2` — usage / parse / load error (malformed input, missing required argument, missing required field)

Implementations MAY refine these (for example: distinct codes for "hash mismatch" vs "tampered envelope"); HESO/1.0 requires the three-level taxonomy above as the minimum.

### §4.8 Reference-implementation extras

The reference implementation (`heso` v0.1.2) ships additional verbs not part of HESO/1.0: `tree`, `ls`, `cat`, `find`, `meta`, `batch`, `eval-js`, `eval-dom`, `search`, `serve`, `fetch`, `action-hash`, `action-hash-verify`, `refresh`, `plat-info`, `plat-diff`, `plat-redact`. These are reference-impl conveniences for agents and operators; they are not required for HESO/1.0 conformance and a second implementation MAY omit them.

Future revisions of this specification MAY promote individual extras into the core catalog via the §4.5 process.

### §4.9 Notes on the catalog

- The verb name `identity-init` appears in the spec as a single hyphenated token to match the §4.3 core wire syntax. The reference CLI exposes it as the two-token form `heso identity init`; implementations MAY also accept the single-token form `heso identity-init` as an alias. The `verb` field of any receipt or plan that references this operation MUST be the single-token `identity-init`.
- `replay` and `run` are distinct: `replay` is pure observation (read the recorded `steps`); `run` re-executes the plan against the embedded cassette. The two are not interchangeable.

---

## §5 Determinism Requirements

### §5.1 Scope

For the purposes of HESO/1.0, an implementation is **deterministic** when, holding the session seed, the plan, and the cassette constant, every observable byte that contributes to `plat_hash` (§1.5) is bit-for-bit identical across:

- repeated runs on the same host,
- runs on different hosts (any OS, any CPU architecture), and
- runs on any other HESO/1.0-conformant implementation.

In scope for §5: seeded entropy exposed to executing JS, virtual clock readings exposed to executing JS, network responses observed by the implementation's HTTP layer, and the firing order of asynchronous callbacks.

Out of scope for §5 (handled normatively in §5.6): GPU rendering output, font fallback, JIT compilation tier, garbage-collector scheduling, ASLR-derived pointer values. HESO/1.0 implementations are not required to constrain these because the HESO/1.0 architecture does not produce or consume them.

Determinism applies only to runs whose `mode` (§5.2) is `deterministic` or to the replay leg of a `recording` run. Live-mode runs are exempt by definition and their receipts MUST be rejected by verifiers (§3.7).

### §5.2 Modes

Every HESO/1.0 session runs in exactly one of three modes. The mode MUST appear as the string-typed `mode` field of any receipt produced from that session (§3.3).

| Mode | Cassette | Entropy | Clock | Receipts |
|---|---|---|---|---|
| `deterministic` | Read-only; misses are fatal | Seeded PRNG (§5.3) | Virtual (§5.4) | Verifiable |
| `recording` | Write; appended in observation order | Seeded PRNG (§5.3) | Virtual (§5.4) | Verifiable |
| `live` | Bypassed | Implementation-defined | Implementation-defined | **MUST be rejected** by verifiers |

Required behavior:

- An implementation MUST support `deterministic` and `recording` modes. `live` mode is OPTIONAL.
- `deterministic` mode MUST NOT issue a live network request under any circumstance. A cassette miss MUST surface as a structured, fatal error to the caller (§2.5) and MUST NOT silently degrade to a live fetch.
- `recording` mode MUST behave identically to `live` mode at the wire while appending every observed exchange to the cassette in capture order, so that the same plan replayed against the produced cassette in `deterministic` mode yields a byte-identical `plat_hash`.
- `live` mode places no determinism requirements on the implementation. An implementation MAY emit receipts from `live` mode but a verifier MUST reject any receipt whose `mode` field is the string `"live"`. The rejection MUST be distinct from a signature-failure outcome (verifier exit codes and error taxonomy are specified in §3).

The three mode strings are part of the wire format and MUST be encoded exactly as `"deterministic"`, `"recording"`, `"live"` (lowercase, no abbreviation, no aliases).

### §5.3 Pseudo-random number generators

In `deterministic` and `recording` modes, every entropy source exposed to executing JavaScript MUST be served from a single seeded PRNG stream derived from the session seed.

#### §5.3.1 Seed type and defaults

- The session seed is an unsigned 64-bit integer. Implementations MUST accept any value in `[0, 2^64 - 1]`.
- The seed value `0` is a real, valid seed — not a sentinel for "no seed." Implementations MUST treat `--seed 0` and a missing `--seed` flag identically, both producing the same reproducible stream.

#### §5.3.2 PRNG algorithm

The PRNG MUST be **ChaCha20** used as a stream cipher in counter mode, seeded per the construction in `rand_chacha::ChaCha20Rng::seed_from_u64` — an internal SplitMix64-style expansion of the `u64` seed into the 32-byte ChaCha20 key, with the counter initialized to zero. ChaCha20 is portable, fully specified, and produces identical streams from identical seeds across all hosts and across implementation languages.

The PRNG choice is normative, not a tuning knob. A future HESO/2.0 MAY revise it; HESO/1.0 implementations MUST use ChaCha20-seeded-from-`u64` exactly as specified.

#### §5.3.3 Surfaces

The following JavaScript-visible entropy sources MUST be served from the single seeded stream, in the order JavaScript draws from them:

| Surface | Output | Draw |
|---|---|---|
| `Math.random()` | `f64` in `[0.0, 1.0)` | One `f64` draw per call, per the `Standard` distribution (53 bits of entropy mapped into the half-open unit interval). |
| `crypto.getRandomValues(view)` | The supplied typed-array `view`, filled in place. Returns the view. | `view.length` bytes (capped at 65 536 per the WebCrypto spec) drawn from the stream and assigned in ascending index order. A view of length 0 MUST NOT advance the stream. |
| `crypto.randomUUID()` | A 36-character lowercase RFC 4122 v4 UUID string. | 16 bytes drawn from the stream, then byte 6 forced to `0x4_` (version 4) and byte 8 forced to `0x[8-b]_` (variant 10), formatted with the canonical 8-4-4-4-12 dash layout. |

All three surfaces MUST draw from the same stream — there is exactly one PRNG instance per session. Implementations MUST NOT shard entropy across per-surface streams.

#### §5.3.4 Cross-implementation reproducibility

Given the same seed and the same JavaScript program, the byte sequence emitted by these three surfaces MUST be identical across every HESO/1.0 implementation. Conformance vectors are tracked under §6.

### §5.4 Virtual clock

In `deterministic` and `recording` modes, every JavaScript-visible source of "current time" MUST read from a single **virtual clock**.

#### §5.4.1 Clock construction

- The clock is an unsigned 64-bit integer count of virtual milliseconds since session start. Sub-millisecond resolution is OUT of scope for HESO/1.0; implementations MUST return integer-valued milliseconds.
- The clock starts at zero on session creation. Implementations MUST NOT seed the clock from the host wall clock.
- The clock advances only under explicit host control (e.g. via the implementation's equivalent of `JsEngine::advance_clock(delta_ms)`). JavaScript code MUST NOT be able to advance the clock by any means other than scheduling a timer and being on the receiving end of a host-driven advance.
- Advances saturate at `u64::MAX`. Implementations MUST NOT wrap.

A future minor version MAY add an optional `epoch_offset_ms` field to plats so a recorded session can replay against a non-zero initial wall-clock time; HESO/1.0 fixes the offset at zero.

#### §5.4.2 Surfaces

| Surface | Reading |
|---|---|
| `Date.now()` | Current virtual-clock reading as a JavaScript `Number` (ms since 1970-01-01T00:00:00Z, offset zero). |
| `new Date()` (zero-arg) | A `Date` constructed as if by `new Date(Date.now())` against the virtual clock. |
| `Date()` (called without `new`) | String form of the zero-arg construction, pinned to the virtual clock. |
| `performance.now()` | Current virtual-clock reading as a `Number`, integer-valued milliseconds. `performance.timeOrigin` MUST be `0`. |
| `setTimeout(cb, delay)` | Schedules `cb` to fire at `now + clamp(delay)` virtual ms. `clamp` per WHATWG HTML: missing / `undefined` / `NaN` / negative / non-finite delays clamp to 0; otherwise truncate toward zero and cap at `2^31 - 1`. |
| `setInterval(cb, period)` | Same scheduling as `setTimeout`, plus re-schedules at `fire_at + max(period, 1)` after each fire. A period of zero is bumped to 1 ms. |
| `requestAnimationFrame(cb)` | Equivalent to `setTimeout(() => cb(performance.now()), 16)`. The returned id MUST be cancellable by both `cancelAnimationFrame` and `clearTimeout`. |
| `clearTimeout(id)`, `clearInterval(id)` | Cancel the timer with `id`; no-op otherwise. Both forms cancel both timer kinds. |

#### §5.4.3 Explicit-input Date forms

`new Date(ms)`, `new Date(dateString)`, `new Date(year, month, day, ...)`, `Date.parse`, and `Date.UTC` are pure functions of their inputs and MUST NOT read the virtual clock. Implementations MUST leave these forms on the underlying JavaScript engine's built-in `Date` without modification.

#### §5.4.4 Firing order

When a host advance fires multiple timers, the firing order MUST be deterministic:

1. Timers MUST fire in ascending `fire_at_ms` order.
2. When two or more timers share a `fire_at_ms`, they MUST fire in ascending **insertion order** (the order in which `setTimeout` / `setInterval` were originally called). This matches WHATWG HTML.
3. A timer callback that throws MUST NOT halt the firing pump. Implementations MUST capture the throw (the reference implementation pushes it to the JavaScript console buffer at error level) and continue firing remaining due timers.
4. A callback that schedules a new timer MUST see the new timer become eligible for firing within the same host advance if its computed `fire_at_ms` is `<= target_ms`. The implementation MUST advance the virtual clock to each firing timer's `fire_at_ms` *before* invoking the callback.

### §5.5 Network input

In `deterministic` mode, every observable network response — every `(method, url, request-body) → (status, response-headers, response-body)` tuple visible to executing JavaScript or to the implementation's HTML/asset fetcher — MUST come from the session's cassette.

Normative requirements:

- An implementation in `deterministic` mode MUST NOT open a network socket for any HTTP, HTTPS, WebSocket, DNS-over-HTTPS, or any other observable network operation initiated by the loaded plan, the parsed HTML, or executing JavaScript. Implementations MAY perform local-only operations (filesystem reads of bundled assets) — these are not observable to a verifier reading the `plat_hash`.
- A cassette miss MUST surface as a structured, fatal error to the caller per §2.5. The error MUST carry the requesting method, URL, and current cassette record count. Implementations MUST NOT silently degrade to a live fetch under any circumstance, **including when an input plat carries no `cassette` field at all** — absence of a cassette in `deterministic` mode is itself a §2.5 miss and MUST fail closed.
- Lookup MUST be byte-exact on URL and request body. Method comparison MAY be case-insensitive. Any URL normalization an implementation chooses to apply MUST be applied identically on both the recording and replay paths so the lookup keys match.
- When the cassette contains multiple records whose `(method, url, request-body)` triples are equal, HESO/1.0 implementations MUST return the first matching record.

In `recording` mode, the network is live, and observed responses MUST be appended to the cassette in the order they were observed. The same `recording`-mode plan re-run against the produced cassette in `deterministic` mode MUST yield a byte-identical `plat_hash`.

### §5.6 Out-of-scope nondeterminism

HESO/1.0 places NO requirements on the following nondeterminism sources, because the HESO/1.0 architecture does not expose them.

| Source | Rationale |
|---|---|
| GPU rendering / sub-pixel rounding / anti-aliasing | HESO/1.0 implementations do not produce raster output. No canvas readback, no `toDataURL`, no WebGL, no screenshots in the plat envelope. |
| Font fallback | HESO/1.0 implementations do not perform CSS layout. |
| JavaScript JIT tier changes / warm-up effects | Timing is read only via the virtual clock; entropy only via the seeded PRNG. Both are independent of the engine's execution-time profile. |
| Garbage-collector scheduling | Same rationale as JIT. |
| Multi-threaded event ordering | HESO/1.0 does not specify a threading model. An implementation introducing parallelism MUST preserve the §5.4.4 firing order from the script's perspective. |
| ASLR-derived pointer values | The agent surface exposes no API that leaks heap addresses. |
| TLS handshake / DNS resolution / HTTP connection reuse timing | Absorbed into the cassette layer (§5.5). |
| Wall-clock-dependent JS the agent did not directly call (e.g. cache-busting `?t=${Date.now()}`) | The wall clock the page reads IS the virtual clock (§5.4). Replays match. |

### §5.7 Conformance

An implementation MUST be considered HESO/1.0 §5-conformant iff, for every test vector in §6, running the vector's plan in `deterministic` mode against the vector's cassette and seed produces:

1. A `plat_hash` byte-identical to the vector's expected hash, AND
2. A receipt whose `mode` field is the string `"deterministic"`, whose embedded seed equals the vector's seed, AND
3. (When the vector exercises entropy or clock surfaces) JavaScript-side observations of `Math.random()`, `crypto.getRandomValues()`, `crypto.randomUUID()`, `Date.now()`, and `performance.now()` byte-identical to the vector's expected sequence.

The reference implementation's `heso-compat-tests` crate carries the conformance corpus. Conformance is binary: any divergence on any vector is non-conformance.

---

## §6 Conformance

A HESO/1.0-conformant implementation MUST:

1. Produce and consume plats matching §1, with `plat_hash` reproducing every vector in §1.9.
2. Produce and consume cassettes matching §2, including the §2.4 `response_body_blake3` round-trip and §2.5 miss semantics.
3. Produce and consume receipts matching §3, with the §3.8 verification order applied exactly as specified.
4. Implement every core verb in §4.7 with the dispatch rules of §4.4.
5. Honor the §5 determinism contract in `deterministic` and `recording` modes; reject `live` receipts on verify per §3.7.

§6 conformance vectors live in the reference implementation's `crates/heso-compat-tests` crate. The current revision pins §1.9 (plat canonicalization) and the determinism-mode invariants of §5; cross-implementation vectors for §3 signing (requires a published test keypair seed) and §5.3 entropy surfaces (requires a published seed sweep) are scheduled for the next revision.

The reference implementation is one realization of HESO/1.0. A specification is fully validated only when a second independent implementation clears the §6 suite; until then, the reference implementation is the operational source of truth for any behavior not yet covered by a vector.

---

## Appendix A: Open questions

The following questions are tracked for future revisions of HESO/1.x or HESO/2.0. None affect HESO/1.0 conformance as written.

- **§3.6 domain-separation prefix.** Adding `"heso-receipt/v1\x00"` to receipt signing payloads aligns with §1.8 envelopes and closes a small confused-deputy risk. Cost: every existing receipt signature becomes invalid.
- **§3.4 full RFC 8785 conformance.** Tighten "subset of JCS" to "full RFC 8785" once second implementations have validated edge cases.
- **Top-level `alg` / `version` tag on receipts.** §1.8 envelopes have one; §3 receipts do not. Adding one is a breaking change.
- **base64 vs base64url.** Standard base64 is pinned for both §1.8 and §3.3.3; a future revision MAY accept base64url as an alias.
- **Cost-field stability.** Reference impl emits zeros for `cost.{bytes,cpu_ms,wall_ms}` today. Threading real accounting through changes the receipt's canonical bytes — a one-time hash drift.
- **`trace_hash` canonicalization unification.** §3.5 uses serde-default; §3.4 uses sort-keys-recursively. Aligning the two simplifies porting; cost is a breaking change.
- **Sequential-cursor replay for repeated cassette keys.** Today HESO/1.0 returns the first matching record; a polling loop replays the same response on every iteration. Sequential-cursor semantics would let polling replay faithfully.
- **`epoch_offset_ms`.** Optional virtual-clock offset for readable `new Date().toISOString()` outputs in receipts. Pure UX; deferred.
- **`crypto.subtle` (WebCrypto async).** Currently out of scope; spec language to follow when reference implementation grows the surface.
- **`fetch()` / XHR determinism in non-cassette `live` sessions.** Live mode is exempt from determinism by §5.2, so this is a UX/clarity question rather than a correctness one.
- **PRNG seed expansion beyond 64 bits.** Implementations may want 256-bit seeds (e.g. directly from a content hash). HESO/1.0 fixes the seed at `u64`.
- **Streaming-message cassette format** (WebSocket / EventSource). §2 covers request/response pairs; a streaming-message cassette is its own design.
- **`application/vnd.heso.plat+json` and `application/vnd.heso.receipt+json` IANA registration** once the spec stabilizes.

---

## Appendix B: References

**Normative:**

- [RFC 2119](https://datatracker.ietf.org/doc/html/rfc2119), [RFC 8174](https://datatracker.ietf.org/doc/html/rfc8174) — Conformance terminology.
- [RFC 8259](https://datatracker.ietf.org/doc/html/rfc8259) — JSON.
- [RFC 8785](https://datatracker.ietf.org/doc/html/rfc8785) — JSON Canonicalization Scheme (used for §1.5).
- [RFC 4648](https://datatracker.ietf.org/doc/html/rfc4648) — Base64 (used for cassette bodies and signature fields).
- [RFC 8032](https://datatracker.ietf.org/doc/html/rfc8032) — Ed25519.
- [RFC 3986](https://datatracker.ietf.org/doc/html/rfc3986) — URI Generic Syntax.
- [BLAKE3 specification](https://github.com/BLAKE3-team/BLAKE3-specs) — 256-bit default output.

**Informative:**

- [WHATWG HTML — timer initialization steps](https://html.spec.whatwg.org/multipage/timers-and-user-prompts.html#timer-initialization-steps) — clamping rules referenced in §5.4.2.
- [WebCrypto API](https://w3c.github.io/webcrypto/) — `Crypto.getRandomValues`, `crypto.randomUUID` referenced in §5.3.3.
- Java packages, Android application IDs, Maven group IDs, OCI image label keys — prior art for reverse-DNS namespacing without a central registry (referenced in §4.2).
- HAR 1.2, WARC 1.1, npm tarball + `integrity` attribute — prior art for inline-bytes wire formats (referenced in §2.1).

**Reference implementation source pointers** (not normative; provided for traceability):

- `crates/heso-cli/src/main.rs` — verb dispatch and tool surface (§4.7).
- `crates/heso-engine-fetch/src/plat.rs` — plat canonicalization and hashing (§1.5, §1.6); §1.9 vector generator.
- `crates/heso-engine-fetch/src/cassette.rs` — cassette wire format (§2).
- `crates/heso-engine-fetch/src/lib.rs` — `tree`, `actions`, and metadata extraction (§1.10, §1.11).
- `crates/heso-engine-js/src/` — virtual clock, PRNG, timer firing pump (§5.3, §5.4).
- `crates/heso-trace/src/lib.rs` — receipt format, signing, verification (§3).
- `crates/heso-cli/src/receipts.rs` — receipt-verify CLI surface, trusted-key allowlist (§3.9).
- `crates/heso-compat-tests/` — §6 conformance corpus.
