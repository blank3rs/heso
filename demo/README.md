# demo/

A small agent that uses `heso` as its tools, with a polished terminal UI.

## Run the agent

```sh
cargo build --release -p heso-cli    # if you haven't already
python demo/agent.py
```

You'll be prompted for a query. The agent decides which heso primitives to call (`search`, `read`, `batch_read`) and answers.

Set `ANTHROPIC_API_KEY` to use a live Claude agent. Without it, the demo runs a scripted `search → batch-read → summarize` pipeline using the same heso primitives.

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
