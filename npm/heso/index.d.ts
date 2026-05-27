// Type declarations for @ixla/heso. Mirrors the JS surface in
// index.js. Everything maps to a real `heso` CLI verb; the JSON
// shapes are documented at https://www.heso.ca/docs.

/**
 * Thrown when the underlying `heso` binary exits non-zero, can't be
 * spawned, or produces stdout that doesn't parse as JSON.
 */
export class HesoError extends Error {
  constructor(
    message: string,
    init?: {
      stdout?: string;
      stderr?: string;
      code?: number | null;
      rpcCode?: number | null;
      command?: string[];
    },
  );
  stdout: string;
  stderr: string;
  /** Process exit code, or `null` if we never managed to spawn the binary. */
  code: number | null;
  /**
   * JSON-RPC error code (e.g. `-32601`), set when the error came from
   * a `Session` (i.e. `heso serve`) call. `null` for subprocess errors.
   */
  rpcCode: number | null;
  /** Snake-case alias for {@link rpcCode} (for cross-language parity). */
  rpc_code: number | null;
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

/** `heso wait <url>` — block until a page condition is satisfied. */
export function wait(url: string, options?: WaitOptions): Promise<Record<string, unknown>>;

/**
 * `heso search <query>` — multi-source web search (DuckDuckGo HTML +
 * Wikipedia REST `summary` by default; optional SearXNG via `searxUrl`).
 * Resolves with `{ query, engines_used, results, knowledge }`. Also
 * available as `registry.search`.
 */
export function search(
  query: string,
  options?: SearchOptions,
): Promise<Record<string, unknown>>;

/**
 * `heso click <url>` — dispatch a real click. Pass `ref` ("@e7") OR a
 * locator option (`text`, `selector`, `ariaLabel`).
 *
 * The resolved JSON carries `url` (the page where the click happened,
 * post any redirects on that page's own fetch), `final_url` (where
 * navigation actually landed after following `<a href>` plus its own
 * redirect chain — equals `url` for non-navigating clicks), and
 * `redirects` (the `{from, to, status}` hops the navigation walked
 * through, empty for direct hits and for clicks that did not
 * navigate).
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

/** `heso tree <url>` — full heading-derived page tree. */
export function tree(url: string, options?: CommonOptions): Promise<Record<string, unknown>>;

/** Options unique to `stamp` / `replay`. */
export interface PlanOptions extends CommonOptions {
  /** Seeds determinism shims (`Math.random`, `crypto.getRandomValues`, timers). */
  seed?: number;
  /** `replay` only: return the plan field instead of the per-step log. */
  plan?: boolean;
  /** `stamp` only: load a v0 plan template from disk. */
  template?: string;
  /** `stamp` only: substitution map for `{{name}}` placeholders in the template. */
  values?: Record<string, string>;
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
 * `heso run <plat.plat>` — re-execute a stamped plat's plan against
 * its embedded cassette. No network: cassette misses error out with
 * structured details. Returns the new plat body whose `plat_hash` must
 * match the input plat byte-for-byte if the cassette was unmodified
 * (ADR 0008). Use {@link replay} for the no-engine inspector that just
 * emits the recorded step log, and {@link stamp} to mint a fresh plat
 * against the live web.
 *
 * Named `runPlat` to avoid collision with the low-level {@link run}
 * escape hatch.
 */
export function runPlat(
  file: string,
  options?: PlanOptions,
): Promise<Record<string, unknown>>;

/** Result shape for `heso refresh`. */
export interface RefreshResult {
  ok: true;
  drifted: boolean;
  input_plat_hash: string;
  live_plat_hash: string;
  /** Present only when `drifted` is true. */
  diff?: { plan_identical: boolean };
}

/**
 * `heso refresh [--seed N] <plat>` — re-stamp a plat against the live
 * web and return drift status. Resolves with `RefreshResult` whether or
 * not drift was detected (the CLI distinguishes via the `drifted`
 * boolean, not the exit code). Rejects with `HesoError` on usage errors
 * (exit 2 — missing plan field, unreachable site).
 */
export function refresh(file: string, options?: PlanOptions): Promise<RefreshResult>;

// Polymorphic verbs ----------------------------------------------------

/** Options for {@link verify}. */
export interface VerifyOptions extends CommonOptions {
  /** Path to a JSON allowlist of base64 pubkeys for sealed envelopes / receipts. */
  trustedKeys?: string;
  /** Require a Time-Stamping Authority countersignature on the envelope. */
  requireTsa?: boolean;
  /** Path to a PEM bundle of trusted TSA root certificates. */
  tsaTrustedRoots?: string;
}

/** Options for {@link info}. */
export interface InfoOptions extends CommonOptions {
  /** `"json"` for parseable output, `"text"` for the human-readable summary. */
  format?: "json" | "text";
  /** Print only the BLAKE3 hash of the plat (mutually exclusive with diff mode). */
  hashOnly?: boolean;
}

/** Options for {@link seal}. */
export interface SealOptions extends CommonOptions {
  /** Identity-key path. Default: `heso-local-data/identity.key`. */
  key?: string;
}

/** Options for {@link unseal}. */
export interface UnsealOptions extends CommonOptions {
  /**
   * When true, stdout is the inner `content` plat body (parsed) instead
   * of the small `{status, alg, public_key, plat_hash}` envelope.
   */
  extract?: boolean;
}

/**
 * `heso verify <file>` — verify integrity and/or signature of a plat,
 * receipt, or sealed envelope. Returns a parsed status object; the
 * shape depends on the input kind.
 */
export function verify(
  file: string,
  options?: VerifyOptions,
): Promise<Record<string, unknown>>;

/**
 * `heso info <file> [<file2>]` — display metadata for a plat, or diff
 * two plats. Pass a single path to inspect one plat; pass a two-element
 * tuple to diff.
 */
export function info(
  fileOrFiles: string | [string, string],
  options?: InfoOptions,
): Promise<Record<string, unknown>>;

/**
 * `heso seal <file> [--key PATH] [--tsa URL] [--no-resign]` — wrap a
 * plat in an Ed25519 envelope (and optionally a TSA countersignature).
 */
export function seal(
  file: string,
  options?: SealOptions,
): Promise<Record<string, unknown>>;

/**
 * `heso unseal <file> [--extract]` — verify a sealed envelope offline.
 * Resolves with the parsed status JSON, or with the extracted inner
 * plat body when `extract: true`.
 */
export function unseal(
  file: string,
  options?: UnsealOptions,
): Promise<Record<string, unknown>>;

// Registry namespace (publish / pull / list / search) ------------------

/** Options for {@link registry.publish}. */
export interface PublishOptions extends CommonOptions {
  /** Required by the CLI — passed through as `-d "…"`. */
  description: string;
  /**
   * Comma-separated tag list passed through as `-t "a,b,c"`. Arrays are
   * joined with `,` for you; empty entries are dropped.
   */
  tags?: string | string[];
}

/** Options for {@link registry.pull}. */
export interface PullOptions extends CommonOptions {
  /** Output path; default is `./<hash>.plat`. Passed through as `-o`. */
  out?: string;
}

/** Options for {@link registry.list}. */
export interface ListOptions extends CommonOptions {
  /** Substring match on description / URL / tags (`-q`). */
  q?: string;
  /** Single-tag filter (`-t`). */
  tag?: string;
  /** Ranking; default `trending` (`--sort`). */
  sort?: "trending" | "downloads" | "newest";
  /** 1..=100, default 20 (`--limit`). */
  limit?: number;
}

/**
 * Registry namespace — `heso registry <publish|pull|list|search>`.
 *
 * `publish` / `pull` / `list` print human-readable banners on stdout
 * and resolve with the raw stdout string; `search` returns parsed JSON.
 * All failures surface as `HesoError`.
 */
export declare const registry: {
  publish(file: string, options: PublishOptions): Promise<string>;
  pull(hash: string, options?: PullOptions): Promise<string>;
  list(options?: ListOptions): Promise<string>;
  search(query: string, options?: SearchOptions): Promise<Record<string, unknown>>;
};

// Identity --------------------------------------------------------------

/**
 * `heso identity <subcommand> [args]` — Ed25519 key management.
 * Today's subcommands are `init` (mint a key) and `show` (print the
 * pubkey). Both accept `[--path P]` (default
 * `heso-local-data/identity.key`) and both emit
 * `{path, public_key, algorithm}` JSON.
 *
 * One typed entry instead of one function per subcommand keeps the
 * surface stable as new subcommands land.
 */
export function identity(
  subcommand: string,
  ...args: string[]
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
