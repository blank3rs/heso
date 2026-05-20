# demo/

A recorded run of Claude Code using heso on a single task: *"what's the top story on Hacker News right now?"*

The demo is intentionally plain — a real terminal session, not a UI wrapper. The agent (`claude -p` with `skills/heso/SKILL.md` auto-discovered) calls heso verbs, the steps stream by, and the answer appears at the end.

Heso is the tool, the harness is the agent — per [AGENTS.md](../AGENTS.md) and ADR 0017.

## Files

- `demo.gif` — what the README embeds
- `demo.cast` — asciinema v2; replay with `asciinema play demo/demo.cast`
- `record.py` — re-records the cast (drives `claude -p --output-format stream-json`, renders each tool call as a clean inline line, finishes with the model's answer)
- `agent.py` — secondary script for the `--no-claude` path (hardcoded `search → batch-read → summary` pipeline, useful if you don't have Claude Code installed)

## Re-record

```sh
cargo build --release -p heso-cli
python demo/record.py --query "what's the top story on Hacker News right now? Use heso. Reply with title, points, and a one-sentence summary."
```

Then convert the cast to a GIF (binary from [asciinema/agg](https://github.com/asciinema/agg) releases):

```sh
agg demo/demo.cast demo/demo.gif --theme monokai --font-size 14 --cols 110 --rows 36
```

## What's intentionally not here

- No rich Python UI wrapper around the recording. The cleaner the visual, the more it reads like a real session — which is what someone watching the README wants to see.
- No Anthropic SDK dependency. The "agent" is Claude Code itself loading the existing heso skill, not a parallel agent loop.
- No benchmark comparisons inline. The demo's job is to show heso working — not to argue against other tools.
