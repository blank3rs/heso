# Changelog

All notable changes to heso are documented here. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/); the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.7] - 2026-05-28

### Added

- `--no-private-networks` flag (and the `HESO_BLOCK_PRIVATE_NETWORKS`
  environment variable) opt into SSRF protection. heso resolves each
  target and refuses the request if any resolved IP is loopback,
  RFC1918 private, link-local (including the `169.254.169.254`
  cloud-metadata address), unspecified, CGNAT (`100.64.0.0/10`), IPv6
  unique-local, or an IPv4-mapped form of any of those. The check runs
  on the resolved address, so an inward-pointing hostname is caught as
  well as a literal IP, and a redirect to a literal private IP is
  refused mid-chain. Off by default so `localhost` stays reachable;
  enable it per call with the flag or process-wide with the env var. A
  blocked request emits `{ok: false, error: {code:
  "private_network_blocked", url}}` and exits 1.
- `--js-timeout <duration>` on `eval-js` and `eval-dom` caps script
  wall-clock time via an interrupt-handler watchdog and returns a
  structured `timeout` error on expiry. Default: no cap.
- `eval-js` / `eval-dom` serialize a DOM-element result as
  `{tag, outerHTML, attrs}` instead of an empty object.

### Changed

- `--best-effort` `partial_reason` gains three values: `bot_challenge`
  now also covers Reddit-style "please wait for verification"
  interstitials; `non_html_content_type` flags a `200 OK` carrying a
  non-HTML body (PDF, JSON, octet-stream, images) instead of treating
  an empty extraction as a clean page; and `http_<code>` reports a
  non-2xx status.
- `eval-js` / `eval-dom` run on a dedicated 8 MB-stack thread, so deep
  recursion trips QuickJS's own guard and returns a structured engine
  error instead of overflowing the OS stack. Serialized eval results
  are capped at 10 MB with a structured error.

### Fixed

- The broken-pipe hook recognizes Windows pipe-closed errors (OS error
  109 / 232) alongside the Unix "Broken pipe" string, so piping a
  verb's output into a reader that closes early (`heso ... | head`)
  exits cleanly on every platform instead of aborting with a panic.
- `verify --trusted-keys` (and `HESO_TRUSTED_KEYS`) fail closed on an
  empty allowlist: zero entries is an error (exit 1), not a
  trust-anyone wildcard.
- Argument errors on the eval and read paths (malformed URL, ASCII
  control characters in a URL, unknown `--include` key, empty search
  query, ref/locator misses) emit a structured `{ok: false, error:
  {code, message}}` envelope on stdout alongside the stderr line. URLs
  containing control characters are rejected rather than silently
  rewritten.
- `stamp` / `run` report an actionable error when a plan action carries
  a CLI-only `--text` / `--selector` / `--aria-label` locator instead
  of a stable `ref`, pointing at `heso find` / `heso read` rather than
  a terse "unknown field" message.

## [0.1.6] - 2026-05-27

### Changed

- Removed internal project-document references from public surfaces.
  The verify-side stderr ("`mode: live` is not replay-safe ..."), the
  cassette-miss errors emitted by the JS `fetch()` and `XMLHttpRequest`
  shims, the Python wrapper docstrings, the TypeScript declarations,
  the README, and the CONTRIBUTING notes no longer cite internal
  project documents. User-facing prose now describes the behavior
  directly.
- README's `heso run` description drops the parenthetical project-doc
  citation; receipts example no longer trails "per <internal-doc>" on
  the live-mode rejection.
- Hygiene: `.pre-commit-config.yaml` and the hygiene workflow's `.md`
  allowlist now admit the four standard meta files
  (`CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`,
  `CHANGELOG.md`). The old `readme-sync-blocks` job (which assumed
  `npm/heso/README.md` lived in the git tree) is replaced with a
  positive assertion that `.github/workflows/pypi.yml` carries the
  publish-time `cp README.md npm/heso/README.md` step. Drift between
  the GitHub-displayed README and the npm-displayed README is now
  structurally impossible — the file is generated at publish time
  from the same root README.

## [0.1.5] - 2026-05-27

### Added

- Global `--timeout <duration>` flag on every network-touching verb
  (`open`, `read`, `click`, `fill`, `submit`, `eval-dom`, `batch`,
  `stamp`, `refresh`, `meta`, `find`, `tree`, `ls`, `cat`). Defaults to
  30 seconds. On timeout the verb emits a structured envelope
  `{ok: false, error: {code: "timeout", timeout_ms, elapsed_ms, url}}`
  and exits 1. `--timeout 0` opts out. The Python and Node wrappers
  install a `timeout + 5s` process-kill backstop.
- Click responses now include `final_url` (where the navigation
  actually landed after following the destination's redirect chain)
  and `redirects[]` (a `{from, to, status}` chain) alongside the
  existing `navigated` / `navigated_to` fields.
- Click, fill, and submit responses now share a unified writing-verb
  envelope: `{ok, op, url, ref, selector, element_id, value, result,
  console, error}`. Selector misses surface as `ok: false` with
  `error.code: "selector_not_matched"`.
- `stamp` step entries carry per-step `status`, `observed` payload,
  and logical `started_at` / `finished_at` timestamps in addition to
  the existing `verb` / `action` / `url_before` / `url_after` fields.
- `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, and this
  changelog at the repo root.

### Changed

- `heso search <query>` is a top-level verb again. The
  `heso registry search ...` form continues to work as the
  registry-namespace alias.
- README rewritten to lead with `eval-dom`, drop the manifest tone,
  and name the verified medium-tier WAF pass-throughs (Zillow,
  Walmart, CoinGecko, LinkedIn anonymous, TripAdvisor, Yahoo Finance,
  old.reddit). The status note now scopes `bot_challenge` honestly to
  the nine WAF needles plus `__cf_chl_opt`.
- npm package README is sourced from the root `README.md` at publish
  time by `scripts/deploy.ps1` and `.github/workflows/pypi.yml`, so
  the GitHub homepage and the npm package can no longer drift
  independently. Stale `unpack` / `plat-*` blocks gone.
- `spec/HESO-1.0.md` is now a thin pointer; the canonical spec lives
  at <https://heso.ca/spec>.
- `heso --help` banner rewritten to match the current dispatch —
  removed stale entries for verbs that were collapsed into the
  polymorphic surface or moved under `heso registry`, and removed
  footer links to internal project documents that aren't part of the
  public repo.
- Engine: response bodies are capped before DOM parsing
  (`engine-js`), and registry / Wikipedia / SearXNG responses are
  capped at 4–16 MiB each.
- Engine: `cli` enforces a wall-clock cap on `open` and `read`.
- `serve`: live-pages store bounded at 32 entries.
- Trace / primitives: `Action` and `PrimitiveOp` inputs now reject
  unknown fields rather than silently dropping them.

### Fixed

- `is_bot_challenge` runs before the HTTP-status branch in
  `partial_reason_for_status`, so Cloudflare / Imperva interstitials
  surface as `partial_reason: "bot_challenge"` regardless of the
  wrapper status (200 / 403 / 429 / 503).
- Ecosystem `pull` now verifies the downloaded plat's BLAKE3 hash
  against the requested content address.
- Module docstring and `cmd_replay` stderr in
  `crates/heso-cli/src/main.rs` no longer reference removed verbs or
  internal-only docs.
- README no longer links to internal project files that aren't
  checked in publicly.
- `SealOptions.tsa` and `SealOptions.noResign` removed from the npm
  TypeScript types (they were declared but never wired through the
  CLI). The Python `seal` docstring drops the same unimplemented
  flags.
- Python wrappers document the `timeout` kwarg on `click`, `fill`,
  `submit`, `meta`, `ls`, `cat`, `find`, `tree`, and `refresh` — the
  flag has worked since the global `--timeout` landed but was missing
  from the docstrings.
- Duplicate `Some("search")` dispatch arm in `crates/heso-cli/src/main.rs`
  removed (the second occurrence was unreachable).

Releases prior to this changelog are documented at
<https://github.com/blank3rs/heso/releases>.
