# CLAUDE.md

Claude Code specific instructions for working in this repository. Loaded automatically alongside the global `~/.claude/CLAUDE.md`.

## Read these on-demand

Instead of auto-loading meta docs every conversation, run the shell command that matches the question. These files are small and `cat`ing them costs near-nothing when you actually need them.

| Question shape | Command |
|---|---|
| Agent entry point / contract / "what is heso" | `cat AGENTS.md` |
| Repo orientation (150-token map) | `cat .agent/MAP.md` |
| What matters this month / next milestone | `cat .agent/PRIORITIES.md` |
| Current focus + next task ID | `jq '.current_focus' state.json` |
| A specific task by ID | `jq '.tasks[] | select(.id == "T-XXX")' state.json` |
| Open questions / blockers | `jq '.open_questions, .blockers' state.json` |
| Crate / Rust conventions | `cat .agent/CONVENTIONS.md` |
| Build, test, release workflows | `cat .agent/WORKFLOWS.md` |
| Glossary (load-bearing terms) | `cat .agent/GLOSSARY.md` |
| How to add a crate | `cat .agent/HOWTO/add-a-crate.md` |
| How to add an ADR | `cat .agent/HOWTO/add-an-adr.md` |
| How to update state.json | `cat .agent/HOWTO/update-state.md` |
| How to use the research wiki | `cat .agent/HOWTO/use-research.md` |
| All ADRs at a glance (with supersession) | `cat decisions/README.md` |
| Load ADR N | `cat decisions/NNNN-*.md` |
| Research wiki index | `cat research/INDEX.md` |
| Specific research note | `cat research/<topic>/<note>.md` |

When in doubt: `cat .agent/MAP.md` first, then drill into whichever path it points at.

## Claude-specific tools available

- **Context7 MCP** (`mcp__context7__*`) — configured at user scope. Use for any external library / crate documentation lookup. See global `~/.claude/CLAUDE.md` for the rule.
- **rust-analyzer LSP** — installed via the `rust-analyzer-lsp@claude-plugins-official` plugin. Use it for go-to-definition, find-references, completion, and rustc/clippy diagnostics on `.rs` files. Zero token cost — runs out-of-process.

## Reflexes (Claude-specific)

- For any Rust crate API question, use Context7 first. The Rust ecosystem moves fast — your training data is probably stale.
- For navigating this codebase, prefer rust-analyzer LSP calls over re-reading source. Faster and structured.
- For long-running work, update [`state.json`](state.json) as you go — don't just batch updates at the end.
- For commits, follow the convention in [`.agent/CONVENTIONS.md`](.agent/CONVENTIONS.md). Co-author tag goes at the end.

## Cross-check docs before implementing

This repo has had real drift between its `.md` files and its ADRs. Doc repetition is *not* consensus — if four files say one thing and one ADR says another, the ADR is usually right (or one of them is stale and nobody noticed). Before you ship code that hardcodes any named thing (a primitive name, a crate name, a flag, a path):

1. Grep the repo for the name. Count distinct spellings.
2. If there is exactly one spelling, ship it.
3. If there is more than one, read each occurrence and find the *source-of-truth* doc (usually an accepted ADR, sometimes a research note that an ADR cites).
4. If the source of truth disagrees with the more numerous mentions, that's drift. **Stop and ask the user which one they want.** Then update the losing copies as part of the same change.

Concrete history: T-020's first pass shipped `click_link` + `click_button` because five files said so. The original design intent (in `research/browser-engines/agent-first-design.md`) and the actual mental model the user wanted (terminal-shaped primitives) were both different. The user caught it; we redesigned in ADR 0010. Don't make the next agent re-learn this lesson.

## Don't

- Don't add a TODO comment without also adding a task to [`state.json`](state.json).
- Don't make an architectural change without an ADR in [`decisions/`](decisions/).
- Don't paraphrase the glossary — terms are load-bearing.
- Don't ship a hardcoded vocabulary (primitive names, status enums, error variants) without doing the cross-check above first.
