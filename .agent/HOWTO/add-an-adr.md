# How to add an ADR

ADR = Architecture Decision Record. Captures a specific decision, the alternatives considered, and the consequences. **Every architectural choice gets one.**

## When to write an ADR

- You are about to make a choice that future maintainers will wonder about.
- You are about to do something that contradicts an existing ADR (write a new one that supersedes the old).
- You picked a tool / library / pattern over reasonable alternatives.
- Anything in `decisions_pending` in `state.json` whose time has come.

## Steps

1. Pick the next number: `ls decisions/` and take `max + 1`, zero-padded to 4 digits.
2. Copy `decisions/0000-template.md` to `decisions/NNNN-short-title.md` (kebab-case title).
3. Fill in every section. If a section truly doesn't apply, write "N/A" — don't delete.
4. Cross-reference: add the ADR number to `links.adr` in any related tasks in `state.json`.
5. If this ADR supersedes a previous one, edit the old ADR to add a `Superseded by: NNNN` line at the top.
6. Commit the ADR in its own commit, separate from code that implements it.

## Format

See [`decisions/0000-template.md`](../../decisions/0000-template.md). Standard sections:

- **Status** — proposed / accepted / superseded / deprecated
- **Context** — what's the problem and what constraints exist
- **Decision** — what we're doing
- **Alternatives** — what we considered and why we didn't pick them
- **Consequences** — what this enables, what it costs, what it forecloses
- **References** — research notes, prior art, docs

## Don't

- Don't write an ADR for a trivial choice (which formatter, which style of doc comment). Save them for architecture.
- Don't write an ADR that's just "we use X". Always include the *why* and the *alternatives*.
- Don't edit an accepted ADR to change the decision. Supersede it with a new one.
