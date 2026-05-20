# demo/

The demo runs Claude Code (`claude -p`) from the repo root so it auto-discovers `skills/heso/SKILL.md` and uses heso verbs as its tools. There is no separate "demo agent" — per [AGENTS.md](../AGENTS.md), heso is agentware and the LLM harness is the agent.

## Run

```sh
cargo build --release -p heso-cli    # if you haven't already
python demo/agent.py
```

You'll be prompted for a query. The wrapper streams Claude Code's tool calls and final answer with a rich terminal UI.

Without Claude Code on PATH, the script falls back to `--no-claude` mode that runs a hardcoded `search -> batch read -> summary` pipeline using heso primitives directly. Useful for a recording when an LLM isn't available; honest about being scripted.

## Record for the README

```pwsh
pwsh demo/record.ps1                                # prompts for query
pwsh demo/record.ps1 "rust web scraping libraries"  # one-shot
```

Captures the desktop with `ffmpeg gdigrab` while the agent runs, then renders an optimized GIF (`demo/demo.gif`) and keeps the raw MP4 (`demo/demo.mp4`).

Requirements: `ffmpeg` in PATH (`winget install Gyan.FFmpeg`) and `python` with `rich` (`pip install rich`). For the live-agent path also: `pip install anthropic`.

## Files

- `agent.py` — the agent loop + UI (Python, `rich`)
- `record.ps1` — recording wrapper (PowerShell, ffmpeg)
- `demo.mp4` / `demo.gif` — produced by `record.ps1` (committed when refreshed)
