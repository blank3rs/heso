// heso — Node library surface for @ixla/heso.
//
// This module is a thin subprocess wrapper around the bundled `heso`
// binary that lives in `@ixla/heso-<platform>-<arch>`. Calling
// `open(url)` spawns the binary with `["open", url]`, captures
// stdout, and parses it as JSON — same contract as the Python
// wrapper.
//
// No FFI, no neon, no N-API addon. Just child_process.spawn + JSON.
// The same binary that `npx @ixla/heso open URL` invokes is the one
// this library spawns programmatically.
//
// Two surfaces:
//
//   - Per-call subprocess (one-shot): `open`, `read`, `search`,
//     `click`, `fill`, `submit`, `evalJs`, `evalDom`, `batch`, `wait`.
//     Each spawns a fresh `heso <verb> ...`, resolves with the
//     parsed JSON, rejects with a `HesoError` on failure.
//
//   - Long-lived session: `new Session()` (or `await session(fn)`)
//     spawns one `heso serve` child and pipes newline-delimited
//     JSON-RPC. Use this when you need cookies / DOM / JS state to
//     persist across calls.
//
// Public CLI shim (`bin/heso.js`) is untouched — `npx @ixla/heso ...`
// still works the same.

"use strict";

const { spawn } = require("child_process");
const path = require("path");
const fs = require("fs");
const { Readable } = require("stream");

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

class HesoError extends Error {
  constructor(message, { stdout = "", stderr = "", code = null, command = [] } = {}) {
    super(message);
    this.name = "HesoError";
    this.stdout = stdout;
    this.stderr = stderr;
    this.code = code;
    this.command = command;
  }
}

// ---------------------------------------------------------------------------
// Binary resolution (mirrors bin/heso.js but exposed for library callers)
// ---------------------------------------------------------------------------

// process.platform x process.arch -> per-platform npm package +
// binary basename. The full matrix as of v0.0.2: Windows x86_64,
// Linux x86_64 + ARM64, macOS Intel + Apple Silicon. Adding a new
// target is a one-line entry here, a sibling `npm/platforms/<plat>-<arch>/`
// directory, and one matrix row in `.github/workflows/pypi.yml`.
const PLATFORMS = {
  "win32 x64": { pkg: "@ixla/heso-win32-x64", bin: "heso.exe" },
  "linux x64": { pkg: "@ixla/heso-linux-x64", bin: "heso" },
  "linux arm64": { pkg: "@ixla/heso-linux-arm64", bin: "heso" },
  "darwin x64": { pkg: "@ixla/heso-darwin-x64", bin: "heso" },
  "darwin arm64": { pkg: "@ixla/heso-darwin-arm64", bin: "heso" },
};

function _platformKey() {
  return `${process.platform} ${process.arch}`;
}

function _findBinary() {
  const key = _platformKey();
  const entry = PLATFORMS[key];
  if (!entry) {
    throw new HesoError(
      `heso: no prebuilt binary for ${key}. ` +
        `Supported: win32-x64, linux-x64, linux-arm64, darwin-x64, darwin-arm64. ` +
        `Track other-platform progress at https://github.com/blank3rs/heso/releases ` +
        `or build from source: cargo install --git https://github.com/blank3rs/heso heso-cli`,
    );
  }

  // 1. Try the per-platform optional dependency. `require.resolve`
  //    with multiple `paths` lets us look upward from several
  //    candidate roots — important for `npm install ../local-path`
  //    setups (which symlink the heso package and so make a naive
  //    resolution miss the consumer's node_modules entirely).
  const lookupRoots = [];
  if (typeof __filename === "string") lookupRoots.push(path.dirname(__filename));
  if (process.cwd()) lookupRoots.push(process.cwd());
  if (require.main && typeof require.main.filename === "string") {
    lookupRoots.push(path.dirname(require.main.filename));
  }
  try {
    const pkgJsonPath = require.resolve(`${entry.pkg}/package.json`, { paths: lookupRoots });
    const binPath = path.join(path.dirname(pkgJsonPath), "bin", entry.bin);
    if (fs.existsSync(binPath)) return binPath;
  } catch (_err) {
    // fall through
  }

  // 1b. Last-ditch upward walk from the cwd looking for
  //     `node_modules/<pkg>/bin/<bin>`. Cheap; useful for monorepos
  //     and hoisted layouts where the per-platform pkg lives further
  //     up the tree than `require.resolve` searched.
  for (const root of lookupRoots) {
    let dir = root;
    for (let i = 0; i < 12; i++) {
      const candidate = path.join(dir, "node_modules", entry.pkg, "bin", entry.bin);
      if (fs.existsSync(candidate)) return candidate;
      const parent = path.dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
  }

  // 2. Fall back to `heso` on PATH (handy in dev / when the platform
  // package was skipped via `npm install --omit=optional`).
  const pathBinary = _whichPath(entry.bin);
  if (pathBinary) return pathBinary;

  throw new HesoError(
    `heso: binary "${entry.bin}" not found. ` +
      `Looked for the @ixla/heso-* platform package and on PATH. ` +
      `Try reinstalling: npm install @ixla/heso`,
  );
}

function _whichPath(name) {
  const PATH = process.env.PATH || "";
  const sep = process.platform === "win32" ? ";" : ":";
  const exts =
    process.platform === "win32"
      ? (process.env.PATHEXT || ".EXE;.CMD;.BAT").split(";")
      : [""];
  for (const dir of PATH.split(sep)) {
    if (!dir) continue;
    for (const ext of exts) {
      const candidate = path.join(dir, name.endsWith(ext) ? name : name + ext);
      try {
        const stat = fs.statSync(candidate);
        if (stat.isFile()) return candidate;
      } catch (_) {
        // not found, keep looking
      }
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// argv assembly
// ---------------------------------------------------------------------------

// Translate one camelCase / snake_case option key into a `--dashed`
// CLI flag. camelCase wins (Node convention).
function _flagName(key) {
  return (
    "--" +
    key
      .replace(/([A-Z])/g, "-$1") // camelCase -> dash
      .replace(/_/g, "-") // snake_case -> dash
      .toLowerCase()
  );
}

// One option value -> zero or more argv tokens. Returns an array so
// we can push spread it into the larger argv.
function _valueArgs(flag, value) {
  if (value === undefined || value === null || value === false) return [];
  if (value === true) return [flag];
  if (Array.isArray(value)) {
    const out = [];
    for (const v of value) {
      if (v === undefined || v === null) continue;
      out.push(flag, String(v));
    }
    return out;
  }
  if (typeof value === "object") {
    return [flag, JSON.stringify(value)];
  }
  return [flag, String(value)];
}

function _optsToArgv(opts) {
  if (!opts) return [];
  const argv = [];
  for (const [key, value] of Object.entries(opts)) {
    const flag = _flagName(key);

    // `field` is the one flag that legitimately repeats.
    if (key === "field" && value && typeof value === "object" && !Array.isArray(value)) {
      // dict form: { name: value, ... }
      for (const [name, v] of Object.entries(value)) {
        argv.push(flag, `${name}=${v}`);
      }
      continue;
    }
    if (key === "field" && Array.isArray(value)) {
      for (const item of value) {
        if (typeof item === "string") argv.push(flag, item);
        else if (Array.isArray(item) && item.length === 2) argv.push(flag, `${item[0]}=${item[1]}`);
        else throw new HesoError(`bad field entry: ${JSON.stringify(item)}`);
      }
      continue;
    }

    argv.push(..._valueArgs(flag, value));
  }
  return argv;
}

// ---------------------------------------------------------------------------
// Core spawn-and-parse
// ---------------------------------------------------------------------------

function _spawn(args, { timeout = 0, binary = null } = {}) {
  const exe = binary || _findBinary();
  const command = [exe, ...args];
  return new Promise((resolve, reject) => {
    const child = spawn(exe, args, { stdio: ["ignore", "pipe", "pipe"], shell: false });

    let stdout = "";
    let stderr = "";
    let timedOut = false;
    let timer = null;

    if (timeout > 0) {
      timer = setTimeout(() => {
        timedOut = true;
        child.kill();
      }, timeout);
    }

    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (c) => {
      stdout += c;
    });
    child.stderr.on("data", (c) => {
      stderr += c;
    });

    child.on("error", (err) => {
      if (timer) clearTimeout(timer);
      reject(
        new HesoError(`failed to spawn ${exe}: ${err.message}`, {
          stdout,
          stderr,
          code: null,
          command,
        }),
      );
    });

    child.on("close", (code, signal) => {
      if (timer) clearTimeout(timer);
      if (timedOut) {
        reject(
          new HesoError(`heso timed out after ${timeout}ms`, {
            stdout,
            stderr,
            code,
            command,
          }),
        );
        return;
      }
      if (code !== 0) {
        const msg = (stderr.trim() || `heso exited with code ${code}`).split("\n")[0];
        reject(new HesoError(msg, { stdout, stderr, code, command }));
        return;
      }
      resolve({ stdout, stderr, code, command });
    });
  });
}

async function _spawnJson(args, opts) {
  const { stdout, stderr, code, command } = await _spawn(args, opts);
  try {
    return JSON.parse(stdout);
  } catch (e) {
    throw new HesoError(`heso stdout did not parse as JSON: ${e.message}`, {
      stdout,
      stderr,
      code,
      command,
    });
  }
}

/**
 * Low-level escape hatch — spawn `heso ARGS` and parse stdout as JSON.
 * Use this to call a CLI subcommand the wrapper doesn't expose yet.
 *
 * @param {string[]} args - positional argv (no leading "heso").
 * @param {{ timeout?: number, parseJson?: boolean, binary?: string }} [opts]
 * @returns {Promise<any>} parsed JSON, or raw stdout string when
 *   `parseJson: false`.
 */
async function run(args, opts = {}) {
  if (opts.parseJson === false) {
    const result = await _spawn(args, opts);
    return result.stdout;
  }
  return _spawnJson(args, opts);
}

// ---------------------------------------------------------------------------
// Typed verbs
// ---------------------------------------------------------------------------

/**
 * `heso open <url>` — fetch a page and resolve with the agent-shaped
 * summary `{ url, title, description, metadata, tree, actions, plat_hash, ... }`.
 */
function open(url, options) {
  return _spawnJson(["open", url, ..._optsToArgv(options)]);
}

/**
 * `heso read <url>` — fetch + run JS + return the full picture
 * `{ title, text, tree, actions, forms, cookies, console, framework, content_hash, ... }`.
 */
function read(url, options) {
  return _spawnJson(["read", url, ..._optsToArgv(options)]);
}

/** `heso search <query>` — multi-backend web search. */
function search(query, options) {
  return _spawnJson(["search", query, ..._optsToArgv(options)]);
}

/** `heso wait <url>` — block until a page condition is satisfied. */
function wait(url, options) {
  return _spawnJson(["wait", url, ..._optsToArgv(options)]);
}

/**
 * `heso click <url> [<@ref> | --text | --selector | --aria-label]`.
 * Pass either `ref` as the second positional (e.g. "@e7") or a locator
 * option (`text`, `selector`, `ariaLabel`).
 */
function click(url, refOrOptions, maybeOptions) {
  // Overload: click(url, "@e7") or click(url, { text: "Sign in" }).
  if (typeof refOrOptions === "string") {
    return _spawnJson(["click", url, refOrOptions, ..._optsToArgv(maybeOptions || {})]);
  }
  return _spawnJson(["click", url, ..._optsToArgv(refOrOptions || {})]);
}

/**
 * `heso fill <url> (<@ref> | --text | --selector | --aria-label) <value>`.
 * Two shapes:
 *   fill(url, "@e3", "hello")
 *   fill(url, "hello", { text: "Email" })
 */
function fill(url, refOrValue, valueOrOptions, maybeOptions) {
  if (typeof valueOrOptions === "string") {
    // fill(url, "@e3", "hello"[, opts])
    return _spawnJson([
      "fill",
      url,
      refOrValue,
      valueOrOptions,
      ..._optsToArgv(maybeOptions || {}),
    ]);
  }
  // fill(url, "hello", { text: "Email" })
  return _spawnJson(["fill", url, ..._optsToArgv(valueOrOptions || {}), refOrValue]);
}

/**
 * `heso submit <url> (<@form-ref> | locator-opts) [--field n=v]... [--data JSON]`.
 */
function submit(url, refOrOptions, maybeOptions) {
  if (typeof refOrOptions === "string") {
    return _spawnJson(["submit", url, refOrOptions, ..._optsToArgv(maybeOptions || {})]);
  }
  return _spawnJson(["submit", url, ..._optsToArgv(refOrOptions || {})]);
}

/** `heso eval-js <js>` — evaluate JS in a sandboxed QuickJS context (no DOM). */
function evalJs(js, options) {
  return _spawnJson(["eval-js", ..._optsToArgv(options), js]);
}

/** `heso eval-dom <url> <js>` — fetch, run page scripts, then eval against the DOM. */
function evalDom(url, js, options) {
  return _spawnJson(["eval-dom", ..._optsToArgv(options), url, js]);
}

/**
 * `heso batch [open|read] <urls...>` — JSON-Lines stdout split into
 * one array of objects, completion-ordered.
 */
async function batch(subverb, urls, options) {
  const args = ["batch", subverb, ..._optsToArgv(options), ...urls];
  const raw = await run(args, { parseJson: false });
  const out = [];
  for (const line of raw.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      out.push(JSON.parse(trimmed));
    } catch (_) {
      // Skip non-JSON banner / progress lines.
    }
  }
  return out;
}

// Optional convenience verbs.
function meta(url, options) {
  return _spawnJson(["meta", url, ..._optsToArgv(options)]);
}
function ls(url, treePath = "/", options) {
  return _spawnJson(["ls", url, treePath, ..._optsToArgv(options)]);
}
function cat(url, target, options) {
  return _spawnJson(["cat", url, target, ..._optsToArgv(options)]);
}
function find(url, options) {
  return _spawnJson(["find", url, ..._optsToArgv(options)]);
}
function fetchUrl(url, options) {
  return _spawnJson(["fetch", url, ..._optsToArgv(options)]);
}
function tree(url, options) {
  return _spawnJson(["tree", url, ..._optsToArgv(options)]);
}

// ---------------------------------------------------------------------------
// Stateful session (wraps `heso serve`)
// ---------------------------------------------------------------------------

/**
 * Long-lived `heso serve` JSON-RPC subprocess. Use for flows that need
 * cookies / DOM / JS state to persist across calls.
 *
 * Use the helper `await session(async (s) => { ... })` for guaranteed
 * cleanup, or manage the lifecycle explicitly with `new Session()` +
 * `s.close()`.
 */
class Session {
  constructor({ binary = null } = {}) {
    this._binary = binary || _findBinary();
    this._idCounter = 0;
    this._pending = new Map();
    this._queue = [];
    this._inflight = false;
    this._buffer = "";
    this._closed = false;
    this._readyPromise = null;
    this._start();
  }

  _start() {
    this._proc = spawn(this._binary, ["serve"], {
      stdio: ["pipe", "pipe", "pipe"],
      shell: false,
    });

    this._proc.stdout.setEncoding("utf8");
    this._proc.stderr.setEncoding("utf8");

    let resolveReady;
    let rejectReady;
    this._readyPromise = new Promise((res, rej) => {
      resolveReady = res;
      rejectReady = rej;
    });

    let seenReady = false;
    this._proc.stdout.on("data", (chunk) => {
      this._buffer += chunk;
      let nl;
      while ((nl = this._buffer.indexOf("\n")) >= 0) {
        const line = this._buffer.slice(0, nl).trim();
        this._buffer = this._buffer.slice(nl + 1);
        if (!line) continue;
        let msg;
        try {
          msg = JSON.parse(line);
        } catch (_) {
          // skip non-JSON garbage
          continue;
        }
        if (!seenReady && msg && msg.method === "ready") {
          seenReady = true;
          resolveReady();
          continue;
        }
        const id = msg && msg.id;
        if (id === undefined || id === null) continue; // stray notification
        const pending = this._pending.get(id);
        if (!pending) continue;
        this._pending.delete(id);
        if (msg.error) {
          pending.reject(
            new HesoError(msg.error.message || "JSON-RPC error", {
              code: msg.error.code,
              command: [this._binary, "serve"],
            }),
          );
        } else {
          pending.resolve(msg.result);
        }
      }
    });

    this._proc.on("error", (err) => {
      if (!seenReady) rejectReady(err);
      for (const { reject } of this._pending.values()) {
        reject(new HesoError(`heso serve errored: ${err.message}`));
      }
      this._pending.clear();
    });

    this._proc.on("close", (code) => {
      this._closed = true;
      if (!seenReady) {
        rejectReady(new HesoError(`heso serve exited (${code}) before ready`));
      }
      for (const { reject } of this._pending.values()) {
        reject(new HesoError(`heso serve closed (${code})`, { code }));
      }
      this._pending.clear();
    });
  }

  async _request(method, params = {}) {
    if (this._closed) throw new HesoError("session is closed");
    await this._readyPromise;
    const id = ++this._idCounter;
    const payload = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
    return new Promise((resolve, reject) => {
      this._pending.set(id, { resolve, reject });
      try {
        this._proc.stdin.write(payload);
      } catch (err) {
        this._pending.delete(id);
        reject(new HesoError(`failed to write to heso serve stdin: ${err.message}`));
      }
    });
  }

  // ----- typed RPC methods -------------------------------------------

  open(url, params = {}) {
    return this._request("open", { url, ...params });
  }
  read(params = {}) {
    return this._request("read", params);
  }
  ls(treePath = "/", params = {}) {
    return this._request("ls", { path: treePath, ...params });
  }
  cat(target, params = {}) {
    return this._request("cat", { target, ...params });
  }
  find(params = {}) {
    return this._request("find", params);
  }
  click(params = {}) {
    return this._request("click", params);
  }
  fill(value, params = {}) {
    return this._request("fill", { value, ...params });
  }
  submit(params = {}) {
    return this._request("submit", params);
  }
  eval(js, params = {}) {
    // Accept both forms:
    //   s.eval("document.title")                — positional string
    //   s.eval({ js: "document.title" })        — options object (matches
    //                                             s.read({...}), s.wait({...}))
    // Picking one fixed shape would surprise users coming from either side.
    if (typeof js === "object" && js !== null && "js" in js) {
      return this._request("eval", { ...js, ...params });
    }
    return this._request("eval", { js, ...params });
  }
  navigate(url, params = {}) {
    return this._request("navigate", { url, ...params });
  }
  wait(params = {}) {
    return this._request("wait", params);
  }
  search(query, params = {}) {
    return this._request("search", { query, ...params });
  }
  ping() {
    return this._request("ping");
  }
  closePage(pageId) {
    return this._request("close", { page_id: pageId });
  }

  /** Terminate the underlying `heso serve` subprocess. */
  async close() {
    if (this._closed) return;
    this._closed = true;
    try {
      if (this._proc.stdin && !this._proc.stdin.destroyed) this._proc.stdin.end();
    } catch (_) {}
    // Best-effort wait for clean exit, then kill.
    await new Promise((resolve) => {
      const timer = setTimeout(() => {
        try {
          this._proc.kill();
        } catch (_) {}
        resolve();
      }, 2000);
      this._proc.once("close", () => {
        clearTimeout(timer);
        resolve();
      });
    });
  }
}

/**
 * `await session(async (s) => { ... })` — guaranteed cleanup wrapper.
 * Or `await session()` to get a raw `Session` you close yourself.
 */
async function session(fn) {
  const s = new Session();
  if (typeof fn !== "function") return s;
  try {
    return await fn(s);
  } finally {
    await s.close();
  }
}

// ---------------------------------------------------------------------------
// Module exports
// ---------------------------------------------------------------------------

module.exports = {
  // verbs
  open,
  read,
  search,
  wait,
  click,
  fill,
  submit,
  evalJs,
  evalDom,
  batch,
  meta,
  ls,
  cat,
  find,
  fetch: fetchUrl,
  tree,
  // session
  Session,
  session,
  // low-level
  run,
  HesoError,
  // for advanced users / tests
  _findBinary,
};
