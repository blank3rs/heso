# Changelog

All notable changes to heso are documented here. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/); the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
  footer links to internal-only ADR files.
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
- README no longer links to ADR files under `decisions/`, which is
  gitignored in the public repo.
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
