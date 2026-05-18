# 0010. Primitives as terminal commands

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay
- **Supersedes:** the *primitive list* section of ADR 0009 (the rest of 0009 — one-tool agent surface, planner → trace runner → primitives → engine layering — stands)

## Context

ADR 0009 locked the public agent surface (one tool: `heso.run`) and named the
internal layers: planner → trace runner → primitives → engine. It also
enumerated twelve primitive operations: `fetch`, `extract`, `fill`,
`click_link`, `click_button`, `submit`, `wait_for`, `screenshot`, `script`,
`cookies`, `storage`, `diff`.

That list had three problems we discovered while implementing it (T-020):

1. **Drift between docs.** The same primitive list appeared in five files
   (ADR 0009, AGENTS.md, PRIORITIES.md, state.json M2 exit criteria, state.json
   T-020 notes) but the older research note (`agent-first-design.md`) and the
   left column of ADR 0009's own ASCII diagram used different names (`click`
   singular, `select`). The GLOSSARY had yet another spelling (`click_by_label`)
   pointing at a `heso-act` crate that never got created. We split `click` into
   `click_link` and `click_button` with no documented rationale — possibly just
   to pad the count to a round 12. None of the names sit in any deeper mental
   model the planner or the LLM-trained-on-shell-sessions could pattern-match
   against.

2. **No unifying metaphor.** Each primitive name was minted independently
   (`fill` vs `cookies` vs `wait_for`), and there was no rule for *adding* a
   new one. When we reached for "read a cookie" or "delete a storage key" we
   had to either add another verb (`get_cookie`, `delete_storage`) or hang
   subcommands off `cookies` and `storage` — neither shape generalizes.

3. **The agent has no orientation surface.** The 0009 list let an agent fill a
   field and click a button but had no equivalent of "where am I, what's
   around me." `pwd` and `ls` are basic to using any shell; in 0009 the
   primitives didn't give the planner a way to *ask the page* what was on it.

The fix is a mental model strong enough that the names write themselves and
the planner can introspect the page the same way the agent would on a
terminal.

## Decision

**The primitives layer is a shell. The current page is the working directory.
Elements are files. The planner emits shell commands; the trace runner
executes them.**

### The fifteen commands

| Op | Terminal analogue | Purpose |
|---|---|---|
| `pwd` | `pwd` | Current URL + page title |
| `ls` | `ls [path]` | List interactable elements (default: current page) or contents of a virtual env scope |
| `cd` | `cd <target>` | Navigate (URL, link element, `..` for back, `-` for previous) |
| `cat` | `cat <path>` | Read element text/attribute or env value |
| `find` | `find -<pred>` | Locate elements matching a predicate (role, name) |
| `grep` | `grep <re>` | Regex-search page text (optionally scoped to one element) |
| `echo` | `echo v > p` | Write a value (fill a field, set a cookie, set a storage key) |
| `rm` | `rm <path>` | Clear / delete (field, cookie, storage key, whole env scope) |
| `click` | (no direct) | Interact with a non-navigating element (button, toggle, custom widget) |
| `submit` | (no direct) | Submit a form by ref |
| `wget` | `wget <url>` | Fetch URL or element resource as raw bytes — also covers images, video, any binary |
| `wait` | (no direct) | Block until a condition holds (element appears, URL contains, fake-clock sleep) |
| `screenshot` | (no direct) | Capture viewport (or one element) as PNG |
| `eval` | `sh -c <src>` | Execute JS in the page context — escape hatch only |
| `diff` | `diff a b` | Diff two page snapshots |

### The `/env/` virtual hierarchy

Cookies and Web Storage are addressable as files under a virtual path:

```
/env/cookie/<name>             # one cookie on the current origin
/env/storage/local/<key>       # one localStorage entry
/env/storage/session/<key>     # one sessionStorage entry
```

Five primitives (`ls`, `cat`, `echo`, `rm`, plus `find`/`grep` if relevant)
operate uniformly over both page elements and env paths. No `cookies` or
`storage` primitive exists; their behavior falls out of the file-system model.

### Why fifteen, not twelve

ADR 0009's "twelve" was a target, not a constraint. Five of the new commands
(`pwd`, `ls`, `find`, `grep`, plus `eval` as escape hatch) didn't exist in
0009 but pay for themselves immediately:

- `pwd` + `ls` give the planner an orientation surface — "where am I, what's
  around me" — that 0009 lacked.
- `find` + `grep` make the planner's job of locating elements an explicit
  primitive instead of an ad-hoc loop.
- `eval` is the JS escape hatch (0009's `script`); same primitive, terminal-
  flavored name.

Three of 0009's primitives consolidated:

- `fetch` → split into `cd` (with navigation) and `wget` (without). The
  semantic distinction is now in the name.
- `click_link` + `click_button` → `cd @link` (navigation) + `click @button`
  (interaction). Same split, but grounded in the shell metaphor.
- `cookies` + `storage` → folded into `cat` / `echo` / `rm` / `ls` over the
  `/env/` hierarchy. One primitive shape, one mental model.

Net delta vs 0009: +5 new (`pwd`, `ls`, `find`, `grep`, `eval` rename), -2
folded (`cookies`, `storage`). `fill` becomes `echo`, `wait_for` becomes
`wait`, `script` becomes `eval` — terminal-flavored renames.

### Why this shape pays off

- **The names are pre-trained.** Every LLM has seen tens of millions of
  terminal sessions. Asking a planner to emit `cat @e3` instead of
  `extract(@e3, "text")` puts us downhill of the model's existing prior.
- **Orientation is first-class.** `pwd` and `ls` exist at the same layer as
  navigation. The agent can always answer "where am I" and "what's here"
  without a custom call shape.
- **The metaphor extends.** When we need to add a new primitive ("read a
  Service Worker registration") we ask "what would the shell command be" and
  usually the name picks itself.
- **Receipts are readable.** A signed trace `[cd, ls, find -role link, cd
  @e7, pwd]` reads like a session history, not a parameter dump. Auditors
  can follow it without a primitive-name decoder.
- **Bulk falls out naturally.** Shell idioms (`for f in $(find ...); do cat $f;
  done`) cover the bulk-extract use case via the trace runner, with no
  bespoke `extract({field: spec, ...})` primitive needed.

## Alternatives considered

- **Keep ADR 0009's list (12 ops, `fetch/extract/fill/click_link/click_button/
  submit/wait_for/screenshot/script/cookies/storage/diff`).** Rejected because
  it doesn't address any of the three problems above — same drift risk, no
  unifying metaphor, no orientation surface.
- **A new bespoke vocabulary (`navigate/inspect/interact/observe/...`).**
  Rejected: no shared prior with how LLMs already think about command-line
  environments; every name is new vocabulary the skill MD has to teach.
- **Mirror Playwright's API verbatim (`goto/click/fill/locator/waitFor/...`).**
  Rejected: Playwright is designed for an imperative test runner written by a
  human, not a trace AST emitted by a planner. The shapes (chained locators,
  page-scoped methods) don't serialize cleanly and don't give the planner an
  orientation primitive.
- **Pure REST verbs (`GET /page`, `PUT /field/@e3`, `DELETE /cookie/sid`).**
  Rejected: REST is read/write over resources; browsing also requires
  interaction (`click`, `submit`, `wait`) and navigation (`cd`) that don't
  match GET/PUT/POST/DELETE cleanly. The shell metaphor is a better fit for
  the actual operation set.
- **One mega-op (`do(plain English request)`).** Rejected: that's what
  `heso.run` already is. The primitives layer is the level where typed,
  determinable, signable operations live. Plain English collapses too much
  to sign.

## Consequences

**Positive:**
- Every primitive name has a built-in mental model — `cat`, `cd`, `ls`, `grep`,
  `echo`, `rm`, `wget`, `find` all carry the right shape for LLMs already.
- Orientation primitives (`pwd`, `ls`) give the planner a way to ask "what
  state am I in" without a custom call shape.
- The `/env/` hierarchy means cookies and storage didn't need bespoke
  primitives — they fall out of the file-system model.
- Signed receipts read like shell sessions, which auditors can follow without
  a decoder.
- Adding a new primitive is now a routine question: "what would the shell
  command be?"

**Negative:**
- Three slightly-leaky abstractions: `click`, `submit`, and `screenshot` have
  no clean terminal analogue. We accept these because the primitives they
  cover are unavoidable in a browser and the names are still short and
  meaningful.
- Fifteen primitives is a third more than 0009's twelve. We accept this
  because the per-primitive complexity is *lower* — five of them
  (`ls`/`cat`/`echo`/`rm`/`find`) reuse the same `/env/` and element targets,
  not bespoke shapes.
- `eval` is a giant escape hatch. It must not become the dominant primitive
  — if the planner reaches for `eval` more than ~5% of the time we have a
  primitive gap to fix.
- Migrating the docs: M2 exit criteria, AGENTS.md, PRIORITIES.md, state.json,
  GLOSSARY.md, the research notes, and the skill MD all referenced the 0009
  list and must be updated. The drift this ADR was triggered by re-occurs in
  reverse if we don't.

## References

- ADR 0009 — one-tool agent surface (still in force; this ADR only supersedes
  its primitive *list*).
- `research/browser-engines/agent-first-design.md` — the original design
  intent that this ADR re-grounds.
- `crates/heso-primitives/src/lib.rs` — the canonical implementation. The
  doc-comment table there is generated from this ADR.
- The user's framing (2026-05-17): "think of the headless browser as a
  terminal and give it all the Linux terminal commands that fit, plus some
  more for img fetching and video; it can go through a website like a
  terminal command in a smart way for it to understand where it is."
