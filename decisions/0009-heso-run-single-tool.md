# 0009. `heso.run` — the single agent-facing tool

- **Status:** Accepted (primitive list section superseded by [ADR 0010](0010-primitives-as-terminal-commands.md))
- **Date:** 2026-05-17
- **Deciders:** Akshay

> **Supersession note (2026-05-17):** the *list* of primitive operations in
> this ADR (and in its architecture diagram) is superseded by
> [ADR 0010 — Primitives as terminal commands](0010-primitives-as-terminal-commands.md).
> Everything else in this ADR — one tool to agents, planner → trace runner →
> primitives → engine layering, status values, receipt shape — is unchanged.
> Read 0010 for the canonical primitive vocabulary; the diagram below is left
> in place as historical context.

## Context

We iterated through several agent-facing API shapes:

1. **Rich semantic API** — `click_by_label`, `fill_field`, `submit_form`, `extract`, etc. (the shape sketched in [`research/browser-engines/agent-first-design.md`](../research/browser-engines/agent-first-design.md)). Agents compose primitives to accomplish tasks. Familiar pattern (Playwright, Stagehand). Downside: many round trips per task, agent still has to plan, every operation is its own potential failure.
2. **Domain-specific language** (e.g. `hesoql`) — agents write declarative queries. Downside: LLMs are weak at brand-new DSLs; teaching the grammar via a skill file is a real tax.
3. **Plain code via an SDK** — agents write Python/JS that calls heso functions. Downside: leaks nondeterminism (`random`, `time.time`, file I/O); auditing a script is much harder than auditing a structured trace.
4. **One tool, plain-English request, structured response.** Agent says what they want; heso does it.

The fourth wins on every axis that matters for an LLM consumer: lowest learning curve, simplest mental model, no syntax to get wrong, agent never picks the wrong primitive, all the complexity moves into heso where we can iterate on it without breaking the agent's contract.

The trade-off is that heso has to ship a **planner** — the component that turns a request + page state into a sequence of primitive operations. That's significant engineering, but it's *bounded* engineering: we own it, we test it, we improve it. The agent's contract stays stable while the planner gets smarter.

## Decision

**heso exposes exactly one public tool to AI agents:**

```
heso.run(start_url: Url, request: String, options?: Options) → Result
```

Where:
- **`start_url`** — where to begin (the page to load, or a previous session handle for continuation)
- **`request`** — a plain-English description of what the agent wants ("get the top 10 stories with title and score", "find the cheapest laptop under $1000", "sign up with this email and password", "watch this page for price changes")
- **`options`** — optional: session seed for determinism, mode (`deterministic` / `recording` / `live`), credentials, timeout, etc.
- **`Result`** — `{ data: <whatever the agent asked for>, receipt: <signed trace>, cost: <bytes/cpu/time>, status: ok|need_clarification|failed }`

That's it. No other public agent-facing tools.

### Internal architecture (everything below the one tool)

```
                    ┌─────────────────────────────┐
agent layer ───►   │       heso.run (MCP tool)   │
                    └─────────────┬───────────────┘
                                  │ request, start_url
                                  ▼
                    ┌─────────────────────────────┐
                    │          planner            │   v0: pattern matching
                    │  request + page  →  trace   │   v1: small in-engine LLM
                    └─────────────┬───────────────┘   v2: fine-tuned planner
                                  │ trace (list of primitive ops)
                                  ▼
                    ┌─────────────────────────────┐
                    │       trace runner          │   deterministic execution
                    │   trace + engine  →  result │   signs the trace
                    └─────────────┬───────────────┘
                                  │ primitive ops
                                  ▼
                    ┌─────────────────────────────┐
                    │       primitives layer      │   ~12 ops:
                    │  fetch / extract / fill /   │     fetch, extract, fill,
                    │  click / submit / wait /    │     click_link, click_button,
                    │  ...                        │     submit, wait_for,
                    └─────────────┬───────────────┘     screenshot, script,
                                  │ EngineApi calls    cookies, storage, diff
                                  ▼
                    ┌─────────────────────────────┐
                    │      EngineApi (Servo)      │   the actual engine
                    └─────────────────────────────┘
```

The primitives layer is where the API previously described in `agent-first-design.md` lives. It is **internal**. Agents never see it.

### The skill MD

heso ships a single markdown file teaching agents how to phrase `request` strings well: what kinds of requests work, how to be specific, how to handle clarification responses, how to chain calls. This is the agent's entire learning curve. ~10 KB, ~20 worked examples.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **Rich semantic API (click/fill/submit/extract as public tools)** | 10–30 tool calls per task; agent still plans; every call is a potential failure with no recovery info; harder to sign and replay than one structured trace. |
| **DSL (hesoql or similar)** | LLMs are demonstrably weaker at unfamiliar grammars; the skill MD becomes a language reference instead of usage examples; first failure mode is "parser rejected your query." |
| **Plain code SDK in Python/JS** | Nondeterminism leaks (`random`, `time`, file I/O, OS calls) defeat ADR 0008; hard to sign arbitrary code; hard to optimize/parallelize opaque branches. |
| **Multiple narrow MCP tools** (one per use case: `scrape`, `submit_form`, `monitor`) | Same fragmentation problem as the rich API; agent picks the wrong tool; surface area grows with every new task type. |
| **No public tool — agents call low-level primitives directly via MCP** | Forces every agent author to write a planner themselves; defeats the whole point of building a browser-for-agents. |

## Consequences

**Positive:**
- Lowest possible agent learning curve. One tool. Plain English.
- All planning complexity lives in heso, where we can fix it once and every agent benefits.
- Determinism (ADR 0008) is enforced at the primitives layer — agents can't accidentally break it.
- Signing (ADR 0005) is trivial: the trace is the artifact that gets signed.
- The agent's contract is stable across heso versions — the internals can rewire as much as they want.
- Cross-LLM portability: any LLM that can write text can use heso.

**Negative:**
- **The planner is now critical-path engineering.** It's the hardest piece. v0 will only handle a bounded set of request patterns; harder requests will fail or require clarification round-trips until v1.
- Failure modes hide: when `heso.run` fails, the agent doesn't see the intermediate state by default. Mitigation: receipts always include the partial trace and what was tried; `options.verbose = true` surfaces detail.
- Higher implementation cost than just exposing primitives. We accept this because the agent's experience is what we're optimizing for.
- Versioning the request semantics. Same English string today and tomorrow should produce the same trace (with same seed + content-addressable inputs). Achieved via versioned planner: each planner version is identified in the receipt.

## References

- [`research/browser-engines/agent-first-design.md`](../research/browser-engines/agent-first-design.md) — the primitives layer's design (now reframed as internals, not agent surface).
- [`research/browser-engines/conversion-strategies.md`](../research/browser-engines/conversion-strategies.md) — three-layer plan (wrap → strip → replace) for shaping Servo into heso; this ADR adds a fourth layer above it: the planner + one tool.
- ADR 0004 (MCP as primary API) — refined: the MCP server exposes one tool (`run`), not many.
- ADR 0005 (Ed25519 identity + signed audit log) — the trace is the artifact being signed.
- ADR 0008 (deterministic execution) — enforced at the primitives layer; agents get it free.
- [Stagehand](https://docs.stagehand.dev/) — closest prior art (natural-language actions on Playwright). Differs from heso in: still primitive-per-call, no built-in determinism, no signing.
