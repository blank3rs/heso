# Changelog

All notable changes to heso are documented here. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/); the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, and this
  changelog at the repo root.

### Changed

- README rewritten to lead with `eval-dom`, drop the manifest tone, and
  name the verified medium-tier WAF pass-throughs (Zillow, Walmart,
  CoinGecko, LinkedIn anonymous, TripAdvisor, Yahoo Finance,
  old.reddit). The status note now scopes `bot_challenge` honestly to
  the nine WAF needles plus `__cf_chl_opt`.
- npm package README replaced the stale `unpack` /
  `plat-info` / `plat-diff` / `plat-redact` / `plat-seal` /
  `plat-unseal` block with the current polymorphic `verify` / `info` /
  `seal` / `unseal` and `registry` surface. The npm README is now
  sourced from the root `README.md` at publish time by
  `scripts/deploy.ps1` and `.github/workflows/pypi.yml`, so the GitHub
  homepage and the npm package no longer drift independently.
- `spec/HESO-1.0.md` is now a thin pointer; the canonical spec lives
  at <https://heso.ca/spec>.
- `heso --help` rewritten to match the current dispatch — removed
  stale entries for verbs that were collapsed into the polymorphic
  surface or moved under `heso registry`, and removed footer links to
  internal-only ADR files.

### Fixed

- `heso search <query>` dispatches at the top level, matching the
  README and the Python wrapper. The `heso registry search ...` form
  continues to work as the registry-namespace alias.
- `is_bot_challenge` now runs before the HTTP-status branch in
  `partial_reason_for_status`, so Cloudflare / Imperva interstitials
  surface as `partial_reason: "bot_challenge"` regardless of the
  wrapper status (200 / 403 / 429 / 503).
- Module docstring and `cmd_replay` stderr in
  `crates/heso-cli/src/main.rs` no longer reference removed verbs or
  internal-only docs.
- README no longer links to ADR files under `decisions/`, which is
  gitignored in the public repo.

Releases prior to this changelog are documented at <https://github.com/blank3rs/heso/releases>.
