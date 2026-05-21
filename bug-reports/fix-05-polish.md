# fix-05-polish — 3 P2 polish bugs (cookies + plat_hash)

Date: 2026-05-21
Branch: `fix/05-polish`
Commits (bottom to top):

- `c150746` — `cookies: per-response snapshot eliminates batch race (bug 04-B)`
- `932e177` — `cookies: emit host_only flag, fill domain for host-only (bug 04-A)`
- `838c1a0` — `plat: hash agent-observable surface, strip per-request UUIDs (bug 05-C)`
- `9497478` — `plat: also strip metadata from hash input (bug 05-C follow-up)`
- `0990ea0` — `docs(cookies): clarify response.cookies() trade-off vs jar scan`

## Bug A — host-only cookies emitted `"domain": ""`

### Files

- `crates/heso-cli/src/main.rs` — `collect_cookies` reshaped to emit
  `{name, value, domain, path, host_only}` (+15 lines including doc).
- `crates/heso-cli/tests/batch.rs` — added
  `batch_read_cookies_emit_host_only_flag_for_no_domain_attribute`
  (~70 lines).

### Fix approach

`collect_cookies` now emits `host_only: true` for cookies where the
captured `ResponseCookie::host_only` flag is set (i.e. the server's
`Set-Cookie` had no `Domain=` attribute — the RFC 6265 §5.3 step 6
host-only default). When `host_only: true`, the `domain` field is
filled with the request URL's effective host (e.g. `"127.0.0.1"`)
instead of the empty string. When `host_only: false`, `domain`
carries the server-sent `Domain=` value.

JSON shape (new field is additive — existing consumers see no change):

```json
{
  "name": "ses",
  "value": "hello",
  "domain": "example.com",
  "path": "/",
  "host_only": false
}
```

vs.

```json
{
  "name": "host_only_cook",
  "value": "H",
  "domain": "127.0.0.1",
  "path": "/",
  "host_only": true
}
```

The `host_only` boolean lives in
`heso_engine_fetch::ResponseCookie` (new struct introduced for bug B);
the CLI just renders the captured per-cookie flag. Detection of
"server sent no `Domain=`" uses
`reqwest::cookie::Cookie::domain().is_none()` per reqwest 0.12 docs
(`response.cookies()` returns an iterator of `reqwest::cookie::Cookie`
whose `domain()` returns `Option<&str>`).

## Bug B — `batch read --include cookies` non-deterministic snapshot

### Files

- `crates/heso-engine-fetch/src/lib.rs` — new public
  `ResponseCookie` struct (`{name, value, domain, path, host_only,
  http_only, secure}`), new `response_cookies: Vec<ResponseCookie>`
  field on `FetchPage`, new private `snapshot_response_cookies` fn,
  `open_static` updated to call it eagerly after `.send().await`
  (+109 lines).
- `crates/heso-cli/src/main.rs` — `collect_cookies` signature changed
  from `(engine: &FetchEngine, url: &Url)` to `(page: &FetchPage)`;
  reads `page.response_cookies` instead of locking the shared jar.
- `crates/heso-cli/src/batch.rs` — call site updated to
  `collect_cookies(&page)`.
- `crates/heso-cli/src/serve.rs` — call site updated to
  `collect_cookies(static_page)`.
- `crates/heso-cli/tests/batch.rs` — added two regression tests
  (`batch_read_cookies_are_per_response_not_jar_snapshot`,
  `batch_read_cookies_are_deterministic_across_runs`, ~110 lines
  combined).

### Fix approach

The old code called `jar.matches(url)` at JSON-serialize time. Under
`--parallel N`, that read happened after sibling tasks had already
written their own `Set-Cookie` responses into the shared jar, so
URL #1's `cookies` field could absorb cookies set by URL #2.

The fix captures the **per-response** cookies eagerly inside
`FetchEngine::open_static` right after `client.send().await`
resolves (before the next `.await`), using
`reqwest::Response::cookies()`. The result is stored as
`Vec<ResponseCookie>` on the `FetchPage`. Because Rust async tasks
don't preempt mid-frame (they only yield at `.await` points), no
sibling can land additional `Set-Cookie` values into the shared jar
between `send().await` returning and the snapshot completing — the
race is closed by construction.

I picked **Approach A** from the bug-report's design options ("Filter
the jar by request URL at receive time — capture only cookies that
match the URL's domain/path AND were set by THIS response") because
the spec explicitly preferred it ("matches what an LLM cares about —
what cookies the response asked for"). The trade-off: cookies set on
302 redirect hops aren't captured by this snapshot (reqwest's
`Response::cookies()` only sees the final response's headers,
confirmed by reading reqwest 0.12.20 source). The follow-up
documentation commit `0990ea0` spells this out so a future reader
who debugs `httpbin.org/cookies/set?X=Y` returning `cookies: []`
finds the explanation in-tree.

## Bug C — `plat_hash` non-deterministic on dynamic pages

### Files

- `crates/heso-engine-fetch/src/plat.rs` — new
  `EPHEMERAL_OBJECT_KEYS` constant (alphabetically sorted, used by
  `is_ephemeral` via `binary_search`), `write_canonical` updated to
  strip ephemeral keys at every JSON-object level. Module-level
  doc-comment rewritten to define `plat_hash` as the
  "agent-observable surface" rather than a byte-fingerprint of the
  response HTML (+90 lines including the new sub-section on what
  the hash names / doesn't name).
- `crates/heso-engine-fetch/src/plat.rs` (follow-up commit
  `9497478`) — added `"metadata"` to the ephemeral key list, with
  explanation that GitHub's `html-safe-nonce`, `request-id`,
  `visitor-hmac`, `visitor-payload`, `fetch-nonce` `<meta>` tags all
  pack per-request entropy into `metadata.meta`.
- `crates/heso-engine-fetch/src/plat.rs` — 7 new unit tests under
  `plat::tests::` proving:
  - `hash_stable_when_only_ephemeral_relational_attrs_change`
  - `hash_still_changes_when_meaningful_attrs_change`
  - `hash_still_changes_when_visible_content_changes`
  - `hash_drops_inline_data_and_data_attrs_top_level_blobs`
  - `hash_drops_per_session_envelope_fields`
  - `hash_preserves_functional_aria_state_attrs`
  - `ephemeral_object_keys_is_sorted_so_binary_search_works`
- `crates/heso-cli/tests/read_verb.rs` — added
  `plat_hash_stable_against_per_request_server_uuids` — wiremock-rs
  integration test using a `Respond` impl that mints a fresh
  `tooltip-<n>` / `btn-<n>` per request and asserts that two
  `heso open` calls produce identical `plat_hash` despite differing
  `actions[0].attrs.id` (~70 lines).

### Fix approach

Redefined what `plat_hash` *names*: the **agent-observable surface**
of a page — `url`, `title`, `description`, `tree`, `actions` (with
relational-pointer attrs filtered), `linked_pages` (recursively
hashed by the same rules). NOT a byte-fingerprint of the response
HTML.

The canonicalizer in `plat::write_canonical` now strips a documented
allowlist of "ephemeral" keys at every level (in addition to
`plat_hash` itself). Two categories:

1. **Element-attribute-level relational pointers** whose values are
   typically server-minted UUIDs cross-referencing other elements:
   `id`, `aria-labelledby`, `aria-describedby`, `aria-controls`,
   `aria-owns`, `aria-activedescendant`, `for`, `nonce`. Functional
   ARIA *state* attributes (`aria-checked`, `aria-disabled`,
   `aria-expanded`, etc.) are NOT stripped — those describe content
   not pointers.
2. **Top-level / mid-tree fields that carry per-request payloads or
   per-session state**: `inline_data`, `data_attrs`, `metadata`
   (added in commit `9497478`), `console`, `cookies`, `scripts`,
   `lazy_hints`, `scroll`, `http_status`, `framework`, failure
   envelope (`partial`, `partial_reason`, `failed_scripts`,
   `console_errors_count`), and derived fields (`content_hash`,
   `delta`, `forms`, `text` — `text` is derived from
   `tree` + post-hydration HTML; `tree.root.intro` carries the
   visible content for hashing).

The list is sorted alphabetically so `is_ephemeral`'s `binary_search`
is O(log n); a guard test
(`ephemeral_object_keys_is_sorted_so_binary_search_works`) catches
future out-of-order insertions.

## Determinism evidence

Built `target/release/heso.exe` against `fix/05-polish` after all
five commits.

### Bug C — `plat_hash` stable across runs

Before fix (from original bug report 05):

```
$ heso open https://github.com/torvalds/linux | jq -r '.plat_hash'
8263f243979c2d84659c28458d78d639ae50a70c6c47183b93b759e17d79d3ae
$ heso open https://github.com/torvalds/linux | jq -r '.plat_hash'
2924c441513ed0ab9da9e8dd8cadec8abe5550c04db37ebb998a9b3ca828208c
```

After fix (verified locally):

```
$ ./target/release/heso.exe open https://github.com/torvalds/linux | jq -r '.plat_hash'
458c59d0c162864cd36a9003fc56db00fe50645fa7f90a003bc06fc8e09ba323
$ ./target/release/heso.exe open https://github.com/torvalds/linux | jq -r '.plat_hash'
458c59d0c162864cd36a9003fc56db00fe50645fa7f90a003bc06fc8e09ba323
```

Identical bytes across two back-to-back runs of a known-dynamic page.

`example.com` (deterministic content) also still hashes deterministically:

```
$ ./target/release/heso.exe open https://example.com/ | jq -r '.plat_hash'
2fcb30e240c364afe669305b4f51ce19f30d9c966bc8cab81e74556b67b38623
$ ./target/release/heso.exe open https://example.com/ | jq -r '.plat_hash'
2fcb30e240c364afe669305b4f51ce19f30d9c966bc8cab81e74556b67b38623
```

### Bug A — host_only flag emitted

Hermetic regression test
`batch_read_cookies_emit_host_only_flag_for_no_domain_attribute`
sets up a wiremock fixture with two cookies on one response:
`host_only_cook=H; Path=/` (no Domain attr) and
`wide_cook=W; Domain=127.0.0.1; Path=/` (explicit Domain). The host-only
entry must have `host_only: true` and `domain: "127.0.0.1"` (the request
host); the domain-wide entry must have `host_only: false` and a non-empty
`domain` containing `"127.0.0.1"`. Passes on `fix/05-polish`.

### Bug B — per-response snapshot is race-free

Hermetic regression tests
`batch_read_cookies_are_per_response_not_jar_snapshot` and
`batch_read_cookies_are_deterministic_across_runs` set up two URLs
each with a distinct `Set-Cookie`, run them at `--parallel 4`, and
assert each row's `cookies` array contains exactly one entry (its
own cookie). The determinism test runs the same fixture twice and
asserts identical per-row counts across runs. Both pass on
`fix/05-polish`.

## Test additions

- `crates/heso-cli/tests/batch.rs`:
  - `batch_read_cookies_are_per_response_not_jar_snapshot`
  - `batch_read_cookies_are_deterministic_across_runs`
  - `batch_read_cookies_emit_host_only_flag_for_no_domain_attribute`
- `crates/heso-cli/tests/read_verb.rs`:
  - `plat_hash_stable_against_per_request_server_uuids`
- `crates/heso-engine-fetch/src/plat.rs` (unit tests under
  `plat::tests`):
  - `hash_stable_when_only_ephemeral_relational_attrs_change`
  - `hash_still_changes_when_meaningful_attrs_change`
  - `hash_still_changes_when_visible_content_changes`
  - `hash_drops_inline_data_and_data_attrs_top_level_blobs`
  - `hash_drops_per_session_envelope_fields`
  - `hash_preserves_functional_aria_state_attrs`
  - `ephemeral_object_keys_is_sorted_so_binary_search_works`

Total: 4 new wiremock-based integration tests + 7 new unit tests = 11
new tests. All pass.

`cargo test --workspace` summary: **1199 tests passed, 0 failed**.

## Constraint-compliance notes

- Granular commits: one per bug, plus follow-ups for the
  metadata-strip refinement and a doc-only clarification.
- `cargo test --workspace` is clean (1199 passed, 0 failed).
- Did NOT touch click/fetch truthfulness, JS engine semantics, DOM
  API additions, or the receipts CLI surface — those domains are
  owned by parallel agents whose in-flight work in this worktree
  was preserved as unstaged modifications when relevant.
- API question lookups: reqwest 0.12 `Response::cookies()` /
  `cookie::Cookie` accessors and redirect-chain semantics were
  confirmed via reqwest's published source at
  `seanmonstar/reqwest@v0.12.20`. `cookie_store 0.21` host-only
  detection via `CookieDomain::HostOnly` was confirmed via the
  crate's docs.rs page.

## Repro commands

```
# Bug C — plat_hash stability across runs of a dynamic page
./target/release/heso.exe open https://github.com/torvalds/linux | jq -r '.plat_hash'
./target/release/heso.exe open https://github.com/torvalds/linux | jq -r '.plat_hash'
# (both must print identical 64-hex-char hashes)

# Bug A + B — hermetic, no real network needed
cargo test -p heso-cli --test batch batch_read_cookies
cargo test -p heso-cli --test read_verb plat_hash_stable
```
