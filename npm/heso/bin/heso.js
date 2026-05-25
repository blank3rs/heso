#!/usr/bin/env node
// Platform-resolver shim for @ixla/heso.
//
// Mirrors the biome / esbuild / swc pattern: this file is the single
// `bin` entry of the meta package. At install time, npm picks the
// matching `@ixla/heso-<platform>-<arch>` optional dependency (filtered
// by `os` and `cpu` in those packages' package.json). At run time, this
// shim finds the embedded binary inside that optional package and execs
// it with the current argv. The user's terminal sees heso's output
// natively (no extra wrapper process, no buffering, no shell quoting).
//
// References:
//   - biome shim:    @biomejs/biome/bin/biome
//   - esbuild shim:  evanw/esbuild npm/esbuild
//   - npm os/cpu:    https://docs.npmjs.com/cli/v11/configuring-npm/package-json#os
//   - optionalDeps:  https://docs.npmjs.com/cli/v11/configuring-npm/package-json#optionaldependencies

"use strict";

const { spawnSync } = require("child_process");
const path = require("path");
const fs = require("fs");

// Map (process.platform, process.arch) -> the npm package that ships
// the matching binary, and the basename of that binary inside the
// package's `bin/` dir. Keep entries narrow; expanding the matrix is a
// one-line edit per target triple.
//
// Current five-platform matrix: Windows x86_64, Linux x86_64 + ARM64,
// macOS Intel + Apple Silicon. Mirror any change here in
// `index.js`'s PLATFORMS map and the matrix in
// `.github/workflows/pypi.yml`.
const PLATFORMS = {
  "win32 x64": { pkg: "@ixla/heso-win32-x64", bin: "heso.exe" },
  "linux x64": { pkg: "@ixla/heso-linux-x64", bin: "heso" },
  "linux arm64": { pkg: "@ixla/heso-linux-arm64", bin: "heso" },
  "darwin x64": { pkg: "@ixla/heso-darwin-x64", bin: "heso" },
  "darwin arm64": { pkg: "@ixla/heso-darwin-arm64", bin: "heso" },
};

function platformKey() {
  return `${process.platform} ${process.arch}`;
}

function resolveBinary() {
  const key = platformKey();
  const entry = PLATFORMS[key];
  if (!entry) {
    return { error: `unsupported-platform`, key };
  }
  // `require.resolve` walks the standard Node lookup, which finds the
  // optional-dependency package wherever npm hoisted it. We resolve the
  // package.json (always present) and then derive the bin path next to
  // it — cleaner than asking for `${pkg}/bin/heso.exe` directly since
  // some package managers (pnpm) symlink differently.
  //
  // The `paths` option keeps the lookup working when the heso package
  // itself is symlinked (e.g. `npm install ./local-path` during dev) —
  // without it, the resolver only walks up from the package's real
  // dir, which is somewhere else entirely from the consumer's
  // node_modules/.
  const lookupRoots = [__dirname, process.cwd()];
  if (require.main && require.main.filename) lookupRoots.push(path.dirname(require.main.filename));
  let pkgJsonPath;
  try {
    pkgJsonPath = require.resolve(`${entry.pkg}/package.json`, { paths: lookupRoots });
  } catch (err) {
    // Last-ditch: walk upward from each candidate root looking for
    // node_modules/<pkg>/bin/<bin>. Catches hoisted / monorepo
    // layouts the require.resolve sweep missed.
    for (const root of lookupRoots) {
      let dir = root;
      for (let i = 0; i < 12; i++) {
        const candidate = path.join(dir, "node_modules", entry.pkg, "bin", entry.bin);
        if (fs.existsSync(candidate)) return { binPath: candidate };
        const parent = path.dirname(dir);
        if (parent === dir) break;
        dir = parent;
      }
    }
    return { error: "missing-platform-package", key, pkg: entry.pkg };
  }
  const binPath = path.join(path.dirname(pkgJsonPath), "bin", entry.bin);
  if (!fs.existsSync(binPath)) {
    return { error: "missing-binary-file", key, pkg: entry.pkg, binPath };
  }
  return { binPath };
}

function main() {
  const resolved = resolveBinary();
  if (resolved.error === "unsupported-platform") {
    process.stderr.write(
      `heso: no prebuilt binary for ${resolved.key}.\n` +
        `Supported: win32-x64, linux-x64, linux-arm64, darwin-x64, darwin-arm64.\n` +
        `Track other-platform progress at https://github.com/blank3rs/heso/releases\n` +
        `or build from source: cargo install --git https://github.com/blank3rs/heso heso-cli\n`,
    );
    process.exit(1);
  }
  if (resolved.error === "missing-platform-package") {
    process.stderr.write(
      `heso: optional dependency "${resolved.pkg}" did not install on ${resolved.key}.\n` +
        `This usually means npm skipped optional packages (e.g. --omit=optional).\n` +
        `Re-run: npm install @ixla/heso\n`,
    );
    process.exit(1);
  }
  if (resolved.error === "missing-binary-file") {
    process.stderr.write(
      `heso: platform package "${resolved.pkg}" is installed but the binary at\n` +
        `  ${resolved.binPath}\n` +
        `is missing. The package may be corrupt — try reinstalling.\n`,
    );
    process.exit(1);
  }

  // `stdio: "inherit"` wires the child's stdin/stdout/stderr straight
  // to ours — so colored output, JSON streams, and prompts all work.
  // We pass the user's argv through unchanged (sans node + this file).
  const result = spawnSync(resolved.binPath, process.argv.slice(2), {
    stdio: "inherit",
    // `shell: false` is the default — keep it that way so quoting and
    // backslashes inside argv don't get re-parsed by cmd.exe / sh.
    shell: false,
  });

  if (result.error) {
    process.stderr.write(`heso: failed to exec binary: ${result.error.message}\n`);
    process.exit(1);
  }
  // `result.signal` is set when the child died on a signal (POSIX).
  // On Windows, status carries the exit code. Pass either upward.
  if (result.signal) {
    process.kill(process.pid, result.signal);
    return;
  }
  process.exit(result.status === null ? 1 : result.status);
}

main();
