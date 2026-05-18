# Glossary

Terms used in this codebase. Precise meanings — don't paraphrase.

## agentware

*(noun)* Applications designed primarily for use by AI agents, not humans. heso's category. Distinguishes from "AI-powered apps" (apps with AI features, still for humans) and "agent frameworks" (libraries for building agents).

## agent

*(noun)* A process consuming heso's API. May be: a coding agent (Claude Code, Codex), a vertical agent (research assistant, scraper), or a script. Has an identity (Ed25519 keypair) once M3 lands.

## engine

*(noun)* The underlying component that fetches and parses pages. Today the binary ships TWO engines side-by-side: `heso-engine-fetch` is the static path — native single-binary Rust (`reqwest` + `html5ever` via `scraper`), no JS execution, no Chrome dep ([ADR 0012](../decisions/0012-fetch-only-native-engine.md)). `heso-engine-js` is the JS path — QuickJS via `rquickjs`, Phase 1A landed (sandboxed evaluator) and Phase 1B in progress (agent-shaped DOM types backed by `scraper::Html`) ([ADR 0014](../decisions/0014-bundled-quickjs-agent-dom.md)). Together they cover the in-scope half of [ADR 0016](../decisions/0016-positioning-headless-browser-for-agents.md): fetch, parse, JS, DOM, forms, clicks, sessions. The `EngineApi` trait in `heso-engine-api` is the swappable abstraction (ADR 0002).

## adapter

*(noun)* A per-host integration package that wraps heso for a specific coding agent or platform (Claude Code plugin, Cursor extension, etc.). Each adapter bundles the MCP server with native skills/hooks/agents so heso feels built-in to that tool.

## session

*(noun)* A logical browser instance with its own cookies, storage, identity, and page state. Sessions can be saved to disk and resumed. Distinct from a tab — one session may hold many tabs.

## primitive

*(noun)* One of the 15 terminal-shaped operations the planner emits and the trace runner executes against the engine. The vocabulary is `pwd`, `ls`, `cd`, `cat`, `find`, `grep`, `echo`, `rm`, `click`, `submit`, `wget`, `wait`, `screenshot`, `eval`, `diff`. **Internal only — agents never call primitives directly; they call `heso.run`.** Lives in `heso-primitives`. See [ADR 0010](../decisions/0010-primitives-as-terminal-commands.md).

## trace

*(noun)* An ordered `Vec<PrimitiveOp>` — the JSON-serializable AST the planner produces and the trace runner consumes. The canonical artifact signed in a receipt. Defined in `heso-primitives` and re-exported by `heso-trace` alongside `Receipt`/`Cost`/`Mode`. Runner lives in `heso-trace-exec`. One trace = one `heso.run` call's plan.

## receipt

*(noun)* The structured record `heso.run` returns under the hood (it's what `result.receipt` carries). Holds the full trace, per-op results, `trace_hash` (BLAKE3 of the canonical JSON of the trace), `pages_seen`, `cost`, `seed`, `mode`, optional `failed_at` + `error`, and (M4+) an Ed25519 signature. Type lives in `heso-trace`; constructed by `heso-trace-exec`'s `run()`.

## working directory (page)

*(noun)* The mental model from [ADR 0010](../decisions/0010-primitives-as-terminal-commands.md): the current page is the working directory; interactable elements are files. `pwd` shows where you are, `ls` shows what's around you, `cd` navigates. The metaphor is what gives the primitives their names.

## env path

*(noun)* A virtual path under `/env/` that addresses cookies and Web Storage as files: `/env/cookie/<name>`, `/env/storage/local/<key>`, `/env/storage/session/<key>`. Read with `cat`, written with `echo`, deleted with `rm`. There is no separate `cookies` or `storage` primitive — the file-system model covers both. See [ADR 0010](../decisions/0010-primitives-as-terminal-commands.md).

## extractor

*(noun)* A function that turns a rendered page into a structured representation an agent can reason about. Examples: `as_markdown`, `tables`, `article`, `forms`. *Planned for the future `heso-extract` crate; not yet implemented and not strictly necessary once the planner has access to `ls`/`cat`/`find`/`grep` primitives.*

## action

*(noun, historical)* In pre-[ADR 0009](../decisions/0009-heso-run-single-tool.md) design (and in [ADR 0001](../decisions/0001-cargo-workspace-layout.md)'s anticipated crate list), an "action" was a semantic agent-facing op such as `click_by_label` / `fill_field` / `submit_form`, planned for a `heso-act` crate. **That design is dead.** Use **primitive** instead, and read [ADR 0009](../decisions/0009-heso-run-single-tool.md) + [ADR 0010](../decisions/0010-primitives-as-terminal-commands.md) for the current model. The `heso-act` crate was never created.

## job

*(noun)* An async operation with a `JobId`. Returned by long-running APIs (page load, action) so agents can poll, cancel, or await without race conditions.

## identity

*(noun)* An Ed25519 keypair representing a specific agent instance. Used to sign actions and audit log entries. Stored in `~/.heso/identity/`. Optionally anchorable to a chain (Solana/Base) for cross-org verification.

## audit log

*(noun)* Append-only, signature-chained record of every action a heso session took. Tamper-evident locally; verifiable by anyone with the agent's public key.

## headless-first

*(adjective)* Designed for non-human use as the default mode, not as a flag. Distinct from "headless" (a normal browser run without a window). heso is headless-first; Chrome `--headless` is not.

## ADR

*(acronym)* Architecture Decision Record. Numbered markdown file in `decisions/`. Captures *what* was decided, *why*, *what alternatives were rejected*, and the *consequences*.

## RFC

*(acronym)* Request For Comments. Markdown file in `proposals/`. Used for changes that need design discussion before any code is written. Promoted to an ADR (or dropped) once decided.
