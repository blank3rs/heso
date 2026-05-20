# `scripts/` — local release driver

This directory contains the hand-driven release pipeline for heso. There
is no CI release path yet — every release is run locally from the
maintainer's Windows machine via `scripts/release.ps1`.

The file `scripts/release.ps1` is **gitignored**. The template
`scripts/release.ps1.example` is committed. To release:

```powershell
# 1. Copy the template (one-off; the copy is gitignored).
Copy-Item scripts\release.ps1.example scripts\release.ps1

# 2. Edit scripts/release.ps1 — fill in $NPM_TOKEN, $PYPI_TOKEN, optional $GITHUB_TOKEN.
# 3. Set $DRY_RUN = $false at the top.

# 4. Run.
powershell -ExecutionPolicy Bypass -File scripts\release.ps1
# or, on PowerShell 7+:
pwsh scripts/release.ps1
```

The script does, in order, with every step honoring `$DRY_RUN`:

1. **Preflight** — required tools on PATH, git clean, on `main`, tag
   `v<workspace-version>` doesn't exist. Auto-installs `maturin` and
   `twine` via `pip install --user` if missing.
2. **Build** — `cargo build --release -p heso-cli`.
3. **Test** — `cargo test --workspace --release`.
4. **Stage** — `dist/heso-<version>-x86_64-pc-windows-msvc.zip`
   containing `heso.exe`, both license files, and a README excerpt.
   Plus `dist/sha256.txt`.
5. **GitHub Release** — `gh release create v<version> dist/*.zip dist/sha256.txt`.
   Uses `gh auth login --with-token` if `$GITHUB_TOKEN` is set,
   otherwise falls back to the existing `gh auth status`.
6. **PyPI** — `maturin build --release --target-dir target/maturin`
   produces a Windows binary wheel (`heso-<version>-py3-none-win_amd64.whl`).
   Then `python -m twine upload --username __token__ --password $PYPI_TOKEN`.
7. **npm** — bumps `npm/platforms/win32-x64/package.json` and
   `npm/heso/package.json` to `<version>`, copies `heso.exe` into the
   per-platform package, writes a scoped `~/.npmrc` for the duration,
   `npm publish` each (platform first, then meta), restores `~/.npmrc`.
8. **Tag + push** — `git tag v<version>` and `git push origin v<version>`.

## Windows-only constraint (v0.0.2)

The first releaseship ships **Windows x86_64 only**. Cross-compiling the
Rust workspace from Windows to Linux / macOS pulls in `cross` /
`zigbuild` toolchains we haven't validated yet. Once a CI matrix lands
(probably `dist generate-ci` per ADR 0018), the platform packages
expand:

```
npm/platforms/
  win32-x64/         ← shipping today
  linux-x64/         ← TODO
  linux-arm64/       ← TODO
  darwin-x64/        ← TODO
  darwin-arm64/      ← TODO
```

Adding a platform is a copy of the `win32-x64/` directory with the new
`os` / `cpu` entries in its `package.json`, plus one line in
`npm/heso/bin/heso.js`'s `PLATFORMS` map and one extra
`optionalDependencies` entry in `npm/heso/package.json`. The release
script already loops over all per-platform packages in spirit — extend
the dispatch when the binaries exist.

## Prereqs

- **cargo** — install Rust via [rustup](https://rustup.rs/).
- **gh** — [GitHub CLI](https://cli.github.com/), authenticated to an
  account with push access to `blank3rs/heso`. Run `gh auth login`
  once, or set `$GITHUB_TOKEN` in the script.
- **python + pip** — Python 3.8+. The script auto-installs `maturin`
  and `twine` if they're missing.
- **node + npm** — npm 8+ recommended (older versions handle
  `optionalDependencies` differently with `os` / `cpu` filtering).
- **npm scope `@ixla`** — must already exist, and the token in
  `$NPM_TOKEN` must have publish access. See
  [ADR 0018](../decisions/0018-distribution-channels.md) for why we
  picked `@ixla` over `@heso`.
- **PyPI account** — must own the `heso` name. See ADR 0018 again.

## Version bumping

The single source of truth is `[workspace.package].version` in the
workspace root `Cargo.toml`. The release script reads it directly, and
the per-package overrides:

- `crates/heso-cli/Cargo.toml` — uses `version.workspace = true`.
- `pyproject.toml` — uses `dynamic = ["version"]`; maturin pulls the
  version from the Rust binary's `Cargo.toml`.
- `npm/heso/package.json` and `npm/platforms/win32-x64/package.json` —
  the release script overwrites these at publish time so they always
  match.

So to cut `v0.0.2`, edit one number, commit, run the script.

## Dry-run

The template ships with `$DRY_RUN = $true`. Running it should print the
full step sequence with no network calls and no file writes outside
`dist/` and `npm/platforms/win32-x64/bin/`. Use it whenever you change
the script.

```powershell
powershell -ExecutionPolicy Bypass -File scripts\release.ps1.example
```
