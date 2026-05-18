# 0007. The .agent/ directory pattern

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay

## Context

heso is agentware — its users are software, including the AI agents that will work *on* this codebase. As of May 2026, the conventions for making a repo agent-friendly are fragmented:

- Each agent has its own preferred file (`CLAUDE.md`, `.cursor/rules/`, `AGENTS.md`, `GEMINI.md`, etc.).
- `AGENTS.md` is winning as the cross-agent project-instructions standard.
- No agent has a standardized place for *agent-oriented meta documentation* — recipes, conventions, glossaries, workflows aimed specifically at AI consumers.

Existing patterns either dump everything into one `README.md` (too long, not skimmable), or hide knowledge in scattered docs and tribal memory (not loadable by an agent landing cold).

## Decision

Create a top-level **`.agent/` directory** that contains all meta documentation designed primarily for AI agents working in this codebase. Contents:

- `MAP.md` — ~150-token always-load orientation: where things live.
- `CONVENTIONS.md` — coding style, error handling, async, docs.
- `WORKFLOWS.md` — build / test / lint / release commands.
- `PRIORITIES.md` — what matters this month; what to explicitly defer.
- `GLOSSARY.md` — load-bearing terms with precise definitions.
- `HOWTO/` — step-by-step recipes for common agent tasks (`add-a-crate.md`, `add-an-adr.md`, `update-state.md`, ...).

`AGENTS.md` at the repo root imports / points to `.agent/` so any agent on any platform that reads `AGENTS.md` gets the full structure.

The dotfile prefix (`.agent/`) keeps it out of casual `ls` output but visible to anyone (or anything) that looks for it.

## Alternatives considered

- **Put everything in `AGENTS.md`.** Rejected: becomes a 5000-token file. Agents would load all of it on every session even when only the glossary is needed.
- **Use `docs/agent/`.** Rejected: `docs/` connotes user-facing docs. Agent meta is structurally different and should be visually separated.
- **Put it in a wiki or external doc.** Rejected: defeats the purpose. Agents work in the repo; meta must be in the repo.
- **Per-agent files (`.claude/`, `.cursor/`, etc.).** Rejected: duplicates content across agents. We want one source of truth, with agent-specific entrypoints (`CLAUDE.md`, etc.) that point into the shared `.agent/`.

## Consequences

**Positive:**
- Agent-friendly meta is first-class, not afterthought.
- New agents (any vendor, any year) get up to speed by reading `AGENTS.md` → `.agent/MAP.md` → relevant `HOWTO/` files.
- Humans benefit too — the structure is just as readable to people.
- Sets a pattern other agentware projects can adopt (`.agent/` could become a convention).

**Negative:**
- One more directory to maintain. Mitigated by keeping each file tight and only updating when behavior changes.
- Dotfile convention may confuse contributors unfamiliar with the pattern. Mitigated by `AGENTS.md` pointing to it explicitly.

## References

- [agents.md](https://agents.md/) — the loose `AGENTS.md` convention adopted by Codex, Cursor, Claude Code, Cline, Copilot, Windsurf.
- [`AGENTS.md`](../AGENTS.md) at repo root.
- [`.agent/MAP.md`](../.agent/MAP.md).
