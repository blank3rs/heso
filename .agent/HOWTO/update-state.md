# How to update state.json

`state.json` is the project's source of truth for what's being worked on. Keep it accurate. **Update as you work, not in bulk at the end.**

## When to update

| Trigger | What to change |
|---------|----------------|
| Started a task | Set its `status` to `in_progress`, update `updated_at`, append a `log` entry. Update `current_focus.next_action_id` if relevant. |
| Finished a task | Set `status` to `done`, update `updated_at`, append a `log` entry. Pick the next task and update `current_focus.next_action_id`. |
| Hit a blocker | Set the task `status` to `blocked`, add a `blocker` entry with `blocking_tasks: ["T-NNN"]`, append a `log` entry. |
| Discovered an unresolved question | Add an `open_questions` entry. Link it to any task it blocks. |
| Need an ADR | Add a `decisions_pending` entry with `needs_adr: true`. |
| Completed a milestone's exit criteria | Set milestone `status` to `achieved`. Move `current_focus` to the next milestone. |
| New work emerged | Add a `tasks` entry. Pick the next free `T-NNN` ID. Link it to the relevant milestone, ADRs, research, and files. |

## How to update (mechanics)

1. Read `state.json` and `state.schema.json`.
2. Edit `state.json` in place.
3. Update `meta.updated_at` (ISO-8601, UTC).
4. Update `meta.updated_by` (your identifier — `claude-opus-4-7`, `akshay`, etc.).
5. Append a `log` entry describing the change.
6. Validate against `state.schema.json` (any JSON-Schema validator works; planned: `cargo xtask state-check`).
7. Commit `state.json` separately from code changes when possible — easier to review.

## ID conventions

- Tasks: `T-001`, `T-002`, ... pad to 3 digits. Never reuse a retired ID.
- Milestones: `M0`, `M1`, ...
- Open questions: `Q-001`, `Q-002`, ...
- Blockers: `B-001`, `B-002`, ...
- Decisions pending: `D-001`, `D-002`, ...
- ADRs: 4-digit, `0001`, `0002`, ...

When picking the next ID, scan the existing entries and pick `max + 1`. Do not gap.

## Cross-linking

Every task should link to whatever helps a future agent pick it up cold:
- `links.adr` — relevant ADR numbers
- `links.research` — research notes (paths relative to `research/`, no `.md` extension)
- `links.files` — code files this task touches
- `links.issues` — GitHub issue URLs
- `links.rfcs` — proposal IDs from `proposals/`

## Don't

- Don't add tasks with no `notes` field if the title is even slightly ambiguous. A cold-reading agent needs context.
- Don't leave `status: in_progress` on a task you're not actively touching.
- Don't update `state.json` without appending a `log` entry — the log is the audit trail.
- Don't ever delete a task — set its `status` to `cancelled` with a `notes` explaining why. History matters.
