# Research

Captured prior art, papers, ecosystem surveys, and exploration notes. **Read before reinventing.**

Research notes are durable — they don't expire, but they do go stale. When a note's claims are no longer current, add a note at the top dating it (don't delete). History matters.

## Layout

| Folder | What's inside |
|--------|---------------|
| `mcp-ecosystem/` | How major coding agents work, MCP convergence patterns, extension surfaces. Drives ADR 0004. |
| `servo-internals/` | Servo crate v0.1.0+ embedding, SpiderMonkey build, Verso prior art. Drives ADR 0003. |
| `existing-agent-browsers/` | Notes on Browserbase, Stagehand, Browser Use, Playwright — what they do, where they fall short, what we'll do differently. |
| `anti-bot/` | TLS fingerprinting, canvas noise, WebDriver detection. Papers and prior art. |
| `extraction-techniques/` | Readability algorithms, accessibility-tree mining, markdown conversion. |

## How to add a research note

See [`.agent/HOWTO/add-research-note.md`](../.agent/HOWTO/add-research-note.md) (TODO — write this when first note is added).

Quick version:

1. Pick the appropriate subfolder (or create one).
2. Name the file kebab-case, `.md`.
3. Top of file: a one-line summary + date.
4. Body: structured notes, link dumps, citations, your own commentary.
5. Cross-link from any relevant ADRs and `state.json` tasks via `links.research`.

## What belongs here vs. elsewhere

- **Research note (here):** captured exploration, third-party docs summaries, papers, comparisons. Things that inform decisions.
- **ADR (`decisions/`):** the decision itself — what we chose and why.
- **Proposal (`proposals/`):** a design proposal that hasn't been decided yet.
- **User docs (`docs/`):** how to use heso. For humans / agent end-users, not contributors.

## Seeded folders (empty placeholders)

The subfolders below are empty pending the M0 research-seeding tasks (T-007 through T-009 in `state.json`). The folders exist as a contract — when the tasks are done, content lands here.
