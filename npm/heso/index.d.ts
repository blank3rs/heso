// Type declarations for @ixla/heso. Mirrors the JS surface in
// index.js. Everything maps to a real `heso` CLI verb; the JSON
// shapes are documented at https://www.heso.ca/docs.

/**
 * Thrown when the underlying `heso` binary exits non-zero, can't be
 * spawned, or produces stdout that doesn't parse as JSON.
 */
export class HesoError extends Error {
  stdout: string;
  stderr: string;
  /** Process exit code, or `null` if we never managed to spawn. */
  code: number | null;
  /** The exact argv (`[binary, ...args]`) we tried to invoke. */
  command: string[];
}

/** Options accepted by most read-only verbs (open/read/search/wait/...). */
export interface CommonOptions {
  /** Wall-clock timeout in milliseconds. 0 = no timeout. */
  timeout?: number;
  /** Override the `heso` binary path. Mainly for tests. */
  binary?: string;
  /** Any other CLI flag (`--my-flag value`); camelCase here -> dashed CLI. */
  [key: string]: unknown;
}

/** Options unique to `open`. */
export interface OpenOptions extends CommonOptions {
  exploreLinks?: number;
  linkCap?: number;
  bestEffort?: boolean;
  injectScript?: string | string[];
}

/** Options unique to `read`. */
export interface ReadOptions extends CommonOptions {
  complete?: boolean;
  include?: string;
  jsFetch?: boolean;
  since?: string;
  bestEffort?: boolean;
  injectScript?: string | string[];
}

/** Options unique to `search`. */
export interface SearchOptions extends CommonOptions {
  limit?: number;
  engines?: string;
  searxUrl?: string;
}

/** Options unique to `wait`. */
export interface WaitOptions extends CommonOptions {
  selectorExists?: string;
  textContains?: string;
  urlMatches?: string;
  networkIdle?: boolean;
  idleWindow?: string;
  time?: string;
}

/** Locator options shared by `click` / `fill` / `submit`. */
export interface LocatorOptions extends CommonOptions {
  text?: string;
  selector?: string;
  ariaLabel?: string;
}

/** Options unique to `submit`. */
export interface SubmitOptions extends LocatorOptions {
  field?: Record<string, string> | Array<[string, string] | string>;
  data?: Record<string, string>;
}

/** Options unique to `batch`. */
export interface BatchOptions extends CommonOptions {
  parallel?: number;
  timeoutPerUrl?: string;
  failFast?: boolean;
  include?: string;
  jsFetch?: boolean;
}

/**
 * `heso open <url>` — fetch a page once and return the agent-shaped
 * summary `{ url, title, description, metadata, tree, actions,
 * plat_hash, ... }`.
 */
export function open(url: string, options?: OpenOptions): Promise<Record<string, unknown>>;

/**
 * `heso read <url>` — fetch + run JS + return the full picture
 * `{ title, text, tree, actions, forms, cookies, console, framework,
 * content_hash, ... }`.
 */
export function read(url: string, options?: ReadOptions): Promise<Record<string, unknown>>;

/** `heso search <query>` — multi-backend web search (DDG + Wikipedia by default). */
export function search(
  query: string,
  options?: SearchOptions,
): Promise<Record<string, unknown>>;

/** `heso wait <url>` — block until a page condition is satisfied. */
export function wait(url: string, options?: WaitOptions): Promise<Record<string, unknown>>;

/**
 * `heso click <url>` — dispatch a real click. Pass `ref` ("@e7") OR a
 * locator option (`text`, `selector`, `ariaLabel`).
 */
export function click(
  url: string,
  ref: string,
  options?: LocatorOptions,
): Promise<Record<string, unknown>>;
export function click(url: string, options: LocatorOptions): Promise<Record<string, unknown>>;

/**
 * `heso fill <url> <ref> <value>` — type into an input.
 *
 *   fill(url, "@e3", "hello")           // positional ref
 *   fill(url, "hello", { text: "..." }) // locator option, value second-positional
 */
export function fill(
  url: string,
  ref: string,
  value: string,
  options?: LocatorOptions,
): Promise<Record<string, unknown>>;
export function fill(
  url: string,
  value: string,
  options: LocatorOptions,
): Promise<Record<string, unknown>>;

/** `heso submit <url>` — submit a form. Accepts a ref or a locator. */
export function submit(
  url: string,
  ref: string,
  options?: SubmitOptions,
): Promise<Record<string, unknown>>;
export function submit(url: string, options: SubmitOptions): Promise<Record<string, unknown>>;

/** `heso eval-js <js>` — evaluate JS in a sandboxed QuickJS context (no DOM). */
export function evalJs(js: string, options?: CommonOptions): Promise<Record<string, unknown>>;

/** `heso eval-dom <url> <js>` — fetch, run page scripts, then eval against the DOM. */
export function evalDom(
  url: string,
  js: string,
  options?: CommonOptions & { seed?: number; jsFetch?: boolean },
): Promise<Record<string, unknown>>;

/**
 * `heso batch [open|read] <urls...>` — parallel multi-URL scrape. The
 * CLI emits JSON-Lines; this wrapper resolves with one array entry
 * per non-empty line, completion-ordered.
 */
export function batch(
  subverb: "open" | "read",
  urls: string[],
  options?: BatchOptions,
): Promise<Array<Record<string, unknown>>>;

/** `heso meta <url>` — structured metadata (JSON-LD, OpenGraph, SEO). */
export function meta(url: string, options?: CommonOptions): Promise<Record<string, unknown>>;

/** `heso ls <url> [path]` — list children at a tree path. */
export function ls(
  url: string,
  path?: string,
  options?: CommonOptions,
): Promise<Record<string, unknown>>;

/** `heso cat <url> <path|@ref>` — read tree intro or element ref. */
export function cat(
  url: string,
  target: string,
  options?: CommonOptions,
): Promise<Record<string, unknown>>;

/** `heso find <url> [--role X] [--name SUBSTR] [--section /p]`. */
export function find(
  url: string,
  options?: CommonOptions & { role?: string; name?: string; section?: string },
): Promise<Record<string, unknown>>;

/** `heso fetch <url>` — raw GET, returns `{ url, text }`. */
export function fetch(url: string, options?: CommonOptions): Promise<Record<string, unknown>>;

/** `heso tree <url>` — full heading-derived page tree. */
export function tree(url: string, options?: CommonOptions): Promise<Record<string, unknown>>;

/** Options unique to `stamp` / `replay`. */
export interface PlanOptions extends CommonOptions {
  /** Seeds determinism shims (`Math.random`, `crypto.getRandomValues`, timers). */
  seed?: number;
}

/**
 * `heso stamp <plan-or-plat>` — execute a plan against the live
 * web and mint a fresh plat that embeds the plan. Accepts a bare
 * `Action[]` JSON array, a plat with a `"plan"` field, or a
 * `TraceFingerprint`. Rejects with `HesoError` if any step failed
 * (the partial plat is still on `error.stdout`).
 */
export function stamp(
  file: string,
  options?: PlanOptions,
): Promise<Record<string, unknown>>;

/**
 * `heso replay <plat.plat>` — read the recorded step log from a plat.
 * Pure observation: no engine, no network, no JS, no cassette lookup.
 * Use the CLI `heso run` verb when you want cassette-backed
 * re-execution.
 */
export function replay(
  file: string,
  options?: PlanOptions,
): Promise<Record<string, unknown>>;

/**
 * `heso unpack <plat.plat>` — extract the `plan` field of a plat for
 * editing. Returns the action array directly. Rejects with
 * `HesoError` when the file has no `plan` field.
 */
export function unpack(file: string): Promise<unknown[]>;

// Plat dev tools + envelope ---------------------------------------------

export interface PlatSealOptions {
  /** Identity-key path. Default: `heso-local-data/identity.key`. */
  key?: string;
}

export interface PlatUnsealOptions {
  /**
   * When true, stdout is the inner `content` plat body (parsed) instead
   * of the small `{status, alg, public_key, plat_hash}` envelope.
   */
  extract?: boolean;
}

/**
 * `heso plat-hash <file>` — BLAKE3 over the plat's canonical-JSON bytes.
 * Returns the 64-char lowercase hex string.
 */
export function platHash(file: string): Promise<string>;

/**
 * `heso plat-verify <file>` — embedded `plat_hash` matches recomputed?
 * Resolves to `true` (CLI exit 0) or `false` (exit 1 = mismatch).
 * Rejects with `HesoError` on usage/file errors (exit 2).
 */
export function platVerify(file: string): Promise<boolean>;

/**
 * `heso plat-info <file>` — human-readable plat summary (multi-line
 * text: `plat_hash`, `verified`, `size`, `url`, `title`, plan/cassette
 * counts, sealed status, partial flag).
 */
export function platInfo(file: string): Promise<string>;

/**
 * `heso plat-diff <a> <b>` — structured diff of two plats.
 * Resolves with `{identical, output}`; `identical` is `true` iff CLI
 * exited 0; `output` is the full stdout.
 */
export function platDiff(
  a: string,
  b: string,
): Promise<{ identical: boolean; output: string }>;

/**
 * `heso plat-redact <field> <file>` — strip a top-level field and emit
 * a new plat with a recomputed `plat_hash`. Removing any present content
 * field changes the hash and invalidates any prior signature. Refuses
 * sealed envelopes (rejects with `HesoError`).
 */
export function platRedact(
  field: string,
  file: string,
): Promise<Record<string, unknown>>;

/**
 * `heso plat-seal <file> [--key PATH]` — Ed25519 envelope wrapper.
 * Default key is `heso-local-data/identity.key`; mint one with
 * `heso identity init`. Returns the parsed `SealedPlat` JSON object
 * (`{alg, content, signature}`).
 */
export function platSeal(
  file: string,
  options?: PlatSealOptions,
): Promise<Record<string, unknown>>;

/**
 * `heso plat-unseal <file> [--extract]` — verify a sealed envelope
 * offline. Resolves with the parsed status JSON
 * (`{status, alg, public_key, plat_hash}`), or with the extracted
 * inner plat body when `extract: true`. Rejects with `HesoError` on
 * exit 1 (`HashMismatch` / `InvalidSignature`) or exit 2
 * (`WrongAlgorithm` / malformed envelope); branch on `err.code`.
 */
export function platUnseal(
  file: string,
  options?: PlatUnsealOptions,
): Promise<Record<string, unknown>>;

/** Low-level: spawn `heso <args>` and parse stdout. */
export function run(
  args: string[],
  opts?: { timeout?: number; parseJson?: boolean; binary?: string },
): Promise<unknown>;

/**
 * Long-lived `heso serve` JSON-RPC subprocess. Use for flows that
 * need cookies / DOM / JS state to persist across calls.
 */
export class Session {
  constructor(opts?: { binary?: string });
  open(url: string, params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  read(params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  ls(path?: string, params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  cat(target: string, params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  find(params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  click(params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  fill(value: string, params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  submit(params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  eval(js: string, params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  navigate(url: string, params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  wait(params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  search(query: string, params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  ping(): Promise<unknown>;
  closePage(pageId: string): Promise<Record<string, unknown>>;
  close(): Promise<void>;
}

/**
 * `await session(async (s) => { ... })` — RAII-style wrapper that
 * closes the underlying subprocess on exit. Returns whatever the
 * callback returns. Pass no callback to get a raw `Session` you close
 * yourself.
 */
export function session(): Promise<Session>;
export function session<T>(fn: (s: Session) => Promise<T> | T): Promise<T>;
