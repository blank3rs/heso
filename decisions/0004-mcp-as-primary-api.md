# 0004. MCP as primary API surface

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay

## Context

heso exposes its capabilities to AI agents. As of May 2026, the **Model Context Protocol (MCP)** has emerged as a de-facto standard for tool-exposure in the agent ecosystem: 7 of 8 major coding agents (Claude Code, OpenAI Codex CLI, Cursor, Cline, Windsurf, Continue, GitHub Copilot, Gemini CLI) support it, with broadly compatible config shapes (`{"mcpServers": {...}}`). Aider is the only major holdout.

Building one MCP server gets heso into nearly every agent tool today, and any new agent tool that ships will almost certainly support MCP.

## Decision

**MCP is the primary API surface for heso.** Capabilities are exposed via a stdio + HTTP MCP server (`heso-mcp` crate). A thin CLI (`heso-cli`) is the secondary surface: it wraps the same internals and provides shell-friendly invocation for agents whose Bash tool is their only path to external tools (notably Aider).

Library crates (`heso-core`, `heso-engine-api`, `heso-extract`, `heso-act`, etc.) are public for embedding into custom Rust projects, but the **expected consumption path is MCP**.

## Alternatives considered

- **CLI-first, no MCP.** Rejected: forces every agent to shell out, loses structured I/O, no async / job semantics, no streaming. CLI is fine as a fallback, not as the primary surface.
- **Language Server Protocol (LSP).** Rejected: LSP is for editor-style code intelligence, not for headless browsing actions. Wrong fit semantically.
- **Custom protocol over WebSocket / gRPC.** Rejected: agents are converging on MCP. Inventing our own protocol means writing adapters for every agent we want to support.
- **Native bindings (Python, Node, etc.) only.** Rejected: works for one ecosystem, locks out others. MCP gives us all of them at once.

## Consequences

**Positive:**
- One implementation reaches Claude Code, Codex, Cursor, Cline, Windsurf, Continue, Copilot, and Gemini CLI.
- Structured I/O, async jobs, streaming, and self-describing tools come for free with MCP.
- Future agents that ship will almost certainly support MCP — our surface scales with the ecosystem.

**Negative:**
- MCP is still young (2024-2026); the spec evolves and we have to track it.
- Aider doesn't support MCP yet. Mitigated by `heso-cli` and an `AGENTS.md` snippet pattern.
- MCP SDK security issues have been disclosed (e.g. April 2026 OX Security RCE class). We must use the Rust SDK carefully and pin to patched versions.

## References

- [Model Context Protocol spec](https://modelcontextprotocol.io)
- [`research/mcp-ecosystem/`](../research/mcp-ecosystem/) — survey of MCP support across coding agents (seeded during M0).
- ADR 0001 (workspace layout) — `heso-mcp` and `heso-cli` are separate crates.
- [rmcp](https://crates.io/crates/rmcp) — Rust MCP SDK.
