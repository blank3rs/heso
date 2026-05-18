# MAP.md

> 150-token orientation. Always load this first when entering the repo.

## Where things live

| Path | What's inside |
|------|---------------|
| `crates/` | Rust crates (8 members). Each does ONE thing. `heso-core`, `heso-engine-api` (trait), `heso-engine-fetch` (native HTTP+HTML, ADR 0012), `heso-engine-js` (QuickJS via rquickjs, ADR 0014 Phase 1A — JS evaluator, no DOM yet), `heso-primitives` (the 15 terminal-shaped ops, ADR 0010), `heso-trace` (Trace + Receipt + Cost + trace_hash), `heso-trace-exec` (trace runner), `heso-cli` (binary). |
| `.agent/` | Meta for AI agents. MAP, CONVENTIONS, WORKFLOWS, PRIORITIES, GLOSSARY, HOWTO/. |
| `decisions/` | ADRs, numbered 0001+. Every architectural choice. Read before changing architecture. |
| `research/` | Captured prior art, papers, ecosystem surveys. Read before reinventing. |
| `proposals/` | RFCs for design discussions that precede code. |
| `docs/` | User-facing long-form docs (later, not seeded yet). |
| `state.json` | Current focus, milestones, tasks, open questions, blockers. **Read this to know what to do next.** |

## Load-bearing files

- `AGENTS.md` — primary agent entry point. Points at everything else; reads other docs on-demand via shell commands (no more `@`-imports).
- `state.json` — what to work on. Schema: `state.schema.json`.
- `decisions/0001-0016` — foundational architecture. Do not violate without a new ADR. Currently in force: 0001 (workspace layout), 0002 (engine trait boundary), 0004 (MCP), 0005 (Ed25519 identity), 0006 (license), 0007 (.agent dir), 0008 (determinism), 0009 (one-tool agent surface), 0010 (terminal primitives, supersedes 0009's primitive list), 0012 (native fetch engine, supersedes 0011 which superseded 0003), 0013 (engine as semantic extractor), 0014 (bundled QuickJS + agent-shaped DOM — Phase 1A landed), 0015 (the plat is the output artefact), **0016 (positioning — headless browser for the agent-relevant half of the web; the lead pitch)**.

## Current phase

See `state.json` → `current_focus` (`jq '.current_focus' state.json`). M0/M1/M2 mostly shipped; cartography V0 landed (ADR 0015); ADR 0014 Phase 1A (QuickJS evaluator) landed 2026-05-18 — DOM types in Phase 1B is the next major lift.

## When confused

1. `jq '.current_focus' state.json` for the summary + `next_action_id`.
2. `cat .agent/PRIORITIES.md` for what matters this month.
3. `cat decisions/README.md` then the ADR most relevant to your task.
4. `cat research/INDEX.md` for prior art on whatever you're about to build.
