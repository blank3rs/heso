#!/usr/bin/env python3
"""
heso agent demo.

Per AGENTS.md and skills/heso/SKILL.md, the agent lives in the LLM
harness (Claude Code), not in heso. This script wires that up for a
recorded demo:

    1. Spawn `claude -p --output-format stream-json` from the repo
       root so Claude Code auto-discovers skills/heso/SKILL.md.
    2. Pipe a query in via stdin.
    3. Render every event verbosely: tool inputs, tool outputs,
       streamed assistant text.
    4. Record everything to demo/demo.cast (asciicast v2). Convert
       to GIF with `agg demo/demo.cast demo/demo.gif`.

Fallback: --no-claude (or no `claude` on PATH) runs heso primitives
directly as a scripted search -> batch read -> summary pipeline.
"""

import argparse
import io
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

if sys.stdout.encoding and sys.stdout.encoding.lower() != "utf-8":
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")

from rich.console import Console, Group
from rich.live import Live
from rich.panel import Panel
from rich.prompt import Prompt
from rich.spinner import Spinner
from rich.syntax import Syntax
from rich.table import Table
from rich.text import Text


REPO_ROOT = Path(__file__).resolve().parent.parent
HESO = REPO_ROOT / "target" / "release" / "heso.exe"
if not HESO.exists():
    HESO = REPO_ROOT / "target" / "release" / "heso"


# ============================================================================
# Asciicast v2 recorder — captures every byte the console writes with timing
# ============================================================================

class CastRecorder(io.TextIOBase):
    """
    Wraps a TextIO stream. Pass-through writes to the underlying stream;
    also append (delta_seconds, text) events. Save as asciicast v2 JSONL.
    """

    def __init__(self, underlying, start_time, width=110, height=40):
        self.underlying = underlying
        self.start_time = start_time
        self.width = width
        self.height = height
        self.events = []

    def writable(self):
        return True

    def write(self, s):
        if s:
            self.events.append([time.time() - self.start_time, "o", s])
        return self.underlying.write(s)

    def flush(self):
        self.underlying.flush()

    def isatty(self):
        return True

    def save(self, path):
        header = {
            "version": 2,
            "width": self.width,
            "height": self.height,
            "timestamp": int(self.start_time),
            "env": {"SHELL": "/bin/bash", "TERM": "xterm-256color"},
        }
        with open(path, "w", encoding="utf-8") as f:
            f.write(json.dumps(header) + "\n")
            for e in self.events:
                f.write(json.dumps(e) + "\n")


# Build a recording console that wraps stdout.
_START = time.time()
_RECORDER = CastRecorder(sys.stdout, _START, width=110, height=44)
console = Console(
    file=_RECORDER,
    width=110,
    force_terminal=True,
    legacy_windows=False,
    color_system="truecolor",
)


# ============================================================================
# UI primitives
# ============================================================================

def banner():
    title = Text()
    title.append("heso", style="bold white on dark_violet")
    title.append("  ", style="")
    title.append("agent demo", style="dim")
    sub = Text(
        "Claude Code drives heso via the existing skill. Tool calls + outputs streamed.",
        style="dim italic",
    )
    console.print(Panel(Group(title, sub), border_style="dark_violet", padding=(0, 2)))


def short(s, n=200):
    s = str(s)
    return s if len(s) <= n else s[: n - 1] + "..."


def render_tool_input(name, payload):
    """Show the verb + every input field. Multi-line for clarity."""
    parts = []
    parts.append(Text("$ ", style="dim") + Text(f"call {name}", style="bold magenta"))
    if isinstance(payload, dict):
        for k, v in payload.items():
            value_str = json.dumps(v, ensure_ascii=False) if not isinstance(v, str) else v
            display = short(value_str, 240)
            parts.append(
                Text(f"   {k}: ", style="dim") + Text(display, style="white")
            )
    return Group(*parts)


def render_tool_result(name, content, elapsed_ms, ok):
    """Show ok/err + first ~12 lines of the actual tool output."""
    head = Text("  ok  ", style="bold green") if ok else Text("  err ", style="bold red")
    head += Text(f"{name}", style="bold")
    head += Text(f"  {elapsed_ms} ms", style="dim")

    # Normalize content into text
    body = ""
    if isinstance(content, str):
        body = content
    elif isinstance(content, list):
        for c in content:
            if isinstance(c, dict) and isinstance(c.get("text"), str):
                body += c["text"]

    # Try to pretty-print if JSON; otherwise show first lines
    body = body.strip()
    pretty = body
    is_json = False
    if body.startswith("{") or body.startswith("["):
        try:
            obj = json.loads(body)
            pretty = json.dumps(obj, indent=2, ensure_ascii=False)
            is_json = True
        except json.JSONDecodeError:
            pass

    # Truncate at 15 lines OR 1200 chars, whichever first
    lines = pretty.splitlines()
    if len(lines) > 15:
        lines = lines[:15] + [f"   ... ({len(pretty.splitlines()) - 15} more lines)"]
    truncated = "\n".join(lines)
    if len(truncated) > 1500:
        truncated = truncated[:1500] + "\n   ..."

    body_renderable = (
        Syntax(truncated, "json", theme="monokai", background_color="default", word_wrap=True)
        if is_json
        else Text(truncated, style="white")
    )
    return Group(head, body_renderable)


# ============================================================================
# Path A: Claude Code (the real agent)
# ============================================================================

def claude_on_path():
    return shutil.which("claude") is not None


def run_via_claude_code(query):
    intro = Text(
        "Spawning claude -p from repo root.\n"
        "It auto-discovers skills/heso/SKILL.md and uses heso verbs as tools.",
        style="dim italic",
    )
    console.print(Panel(intro, border_style="dim"))
    console.print()

    cmd = [
        "claude",
        "-p",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
        "--allowed-tools",
        # Cover bare `heso` (PATH), unix-style relative, Windows-style
        # backslash relative, and the explicit .exe form.
        "Bash(heso:*),Bash(./target/release/heso:*),Bash(./target/release/heso.exe:*),Bash(.\\target\\release\\heso.exe:*),Bash(.\\target\\release\\heso:*)",
    ]
    proc = subprocess.Popen(
        cmd,
        cwd=str(REPO_ROOT),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    proc.stdin.write(query)
    proc.stdin.close()

    tool_starts = {}  # tool_use_id -> start_time
    seen_text_blocks = set()  # message_id+block_index -> already emitted

    streaming_text = {}  # (msg_id, idx) -> accumulated string
    streaming_live = None
    streaming_key = None

    def stop_stream_panel():
        nonlocal streaming_live, streaming_key
        if streaming_live is not None:
            streaming_live.stop()
            streaming_live = None
            streaming_key = None

    final_text = []

    for line in proc.stdout:
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue

        etype = ev.get("type")

        # ---------- streaming partial text ----------
        if etype == "stream_event":
            inner = ev.get("event", {})
            inner_type = inner.get("type")
            if inner_type == "content_block_delta":
                delta = inner.get("delta", {})
                if delta.get("type") == "text_delta":
                    text = delta.get("text", "")
                    key = (ev.get("parent_tool_use_id"), inner.get("index"))
                    streaming_text.setdefault(key, "")
                    streaming_text[key] += text
                    if streaming_key != key:
                        stop_stream_panel()
                        streaming_key = key
                        streaming_live = Live(
                            Panel(Text(streaming_text[key], style="white"), title="agent", border_style="cyan", padding=(0, 1)),
                            console=console,
                            refresh_per_second=12,
                            transient=False,
                        )
                        streaming_live.start()
                    else:
                        streaming_live.update(
                            Panel(Text(streaming_text[key], style="white"), title="agent", border_style="cyan", padding=(0, 1))
                        )
            elif inner_type == "content_block_stop":
                stop_stream_panel()
            continue

        # ---------- assistant message (complete) ----------
        if etype == "assistant" and isinstance(ev.get("message"), dict):
            stop_stream_panel()
            for block in ev["message"].get("content", []):
                btype = block.get("type")
                if btype == "text" and block.get("text", "").strip():
                    final_text.append(block["text"].strip())
                elif btype == "tool_use":
                    name = block.get("name", "?")
                    inputs = block.get("input", {}) or {}
                    console.print(render_tool_input(name, inputs))
                    tool_starts[block.get("id")] = time.time()
            continue

        # ---------- tool result (comes as user-role message) ----------
        if etype == "user" and isinstance(ev.get("message"), dict):
            stop_stream_panel()
            for block in ev["message"].get("content", []):
                if block.get("type") == "tool_result":
                    tid = block.get("tool_use_id")
                    started = tool_starts.get(tid, time.time())
                    elapsed_ms = int((time.time() - started) * 1000)
                    ok = not block.get("is_error", False)
                    # Get the tool name by looking up the matching tool_use
                    name = "tool"
                    for older in ev.get("message", {}).get("content", []):
                        pass  # tool_result blocks don't include the name
                    console.print(render_tool_result(name, block.get("content"), elapsed_ms, ok))
                    console.print()
            continue

        # ---------- result (final) ----------
        if etype == "result":
            stop_stream_panel()
            # Final assistant text comes here via "result" field
            text = ev.get("result", "").strip()
            if text and text not in final_text:
                final_text.append(text)

    stop_stream_panel()
    proc.wait(timeout=30)

    if proc.returncode != 0:
        err = proc.stderr.read() if proc.stderr else ""
        console.print(Panel(Text(f"claude -p exited {proc.returncode}\n{err}", style="red"), border_style="red"))
        return

    if final_text:
        last = final_text[-1].strip()
        console.print(Panel(Text(last, style="white"), title="final answer", border_style="green", padding=(1, 2)))


# ============================================================================
# Path B: offline (no LLM) — just demonstrate the verbs
# ============================================================================

def run_heso(args, timeout=60):
    if not HESO.exists():
        return False, None, f"heso binary not found at {HESO}"
    proc = subprocess.run(
        [str(HESO), *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
        cwd=str(REPO_ROOT),
    )
    out = (proc.stdout or "").strip()
    err = (proc.stderr or "").strip()
    try:
        return proc.returncode == 0, json.loads(out), err, out
    except json.JSONDecodeError:
        return proc.returncode == 0, out, err, out


def offline_demo(user_input):
    console.print(
        Panel(
            Text(
                "Offline demo: no LLM in the loop.\n"
                "Runs heso primitives directly: search -> batch read -> summary.",
                style="dim italic",
            ),
            border_style="dim",
        )
    )
    console.print()

    # ---- search ----
    console.print(render_tool_input("heso search", {"query": user_input, "limit": 5}))
    start = time.time()
    ok, data, _, raw = run_heso(["search", user_input, "--limit", "5"])
    elapsed = int((time.time() - start) * 1000)
    console.print(render_tool_result("heso search", raw, elapsed, ok))
    console.print()

    results = (data or {}).get("results", []) if isinstance(data, dict) else []
    urls = [r["url"] for r in results[:3]]
    if not urls:
        console.print(Panel(Text("no results", style="red"), border_style="red"))
        return

    # ---- batch read ----
    console.print(render_tool_input("heso batch read", {"urls": urls, "parallel": 2}))
    start = time.time()
    proc = subprocess.run(
        [str(HESO), "batch", "read", "--parallel", "2", *urls],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=120,
    )
    elapsed = int((time.time() - start) * 1000)
    # Show a summary line per result instead of the raw multi-line JSON dump
    summary = []
    for line in (proc.stdout or "").splitlines():
        if not line.strip():
            continue
        try:
            d = json.loads(line)
            if "error" in d:
                summary.append(f'  ERR {d.get("url","")}: {d["error"]}')
            else:
                title = (d.get("title") or "")[:70]
                actions = len(d.get("actions") or [])
                summary.append(f'  OK  {d.get("url","")}\n      title={title!r} actions={actions}')
        except json.JSONDecodeError:
            continue
    summary_text = "\n".join(summary)
    console.print(render_tool_result("heso batch read", summary_text, elapsed, proc.returncode == 0))
    console.print()

    # ---- summary panel ----
    bits = [Text("Pages found:", style="bold")]
    for line in summary_text.split("\n"):
        bits.append(Text(line, style="white"))
    console.print(Panel(Group(*bits), title="final answer", border_style="green", padding=(1, 2)))


# ============================================================================
# Entrypoint
# ============================================================================

def main():
    banner()
    if not HESO.exists():
        console.print(
            Panel(
                Text(
                    f"heso binary not found at {HESO}\n"
                    f"run: cargo build --release -p heso-cli",
                    style="red",
                ),
                border_style="red",
            )
        )
        sys.exit(1)

    parser = argparse.ArgumentParser()
    parser.add_argument("--query", help="Skip the prompt; use this query")
    parser.add_argument("--no-claude", action="store_true", help="Skip Claude Code; run the heso-only pipeline")
    parser.add_argument("--save-svg", help="Save a styled SVG of the session here")
    parser.add_argument("--save-cast", help="Save an asciicast (.cast) of the session here")
    cli_args, _ = parser.parse_known_args()

    if cli_args.query:
        user_input = cli_args.query
        console.print(f"[bold]ask the agent[/bold]: {user_input}")
    else:
        try:
            user_input = Prompt.ask("[bold]ask the agent[/bold]")
        except (KeyboardInterrupt, EOFError):
            console.print()
            return
    if not user_input.strip():
        return

    console.print()
    start = time.time()
    if cli_args.no_claude or not claude_on_path():
        offline_demo(user_input)
    else:
        run_via_claude_code(user_input)
    elapsed = int(time.time() - start)
    console.print(Text(f"done in {elapsed}s", style="dim"))

    if cli_args.save_svg:
        out = Path(cli_args.save_svg)
        out.parent.mkdir(parents=True, exist_ok=True)
        # rich's record requires a recording console; we use the cast recorder
        # for animation, so SVG falls out of the cast at the end via text.
        # We just dump the final state via export_text into a styled svg.
        console.print(f"[dim]note: --save-svg disabled in cast-recording build; use --save-cast and convert with agg[/dim]")

    if cli_args.save_cast:
        out = Path(cli_args.save_cast)
        out.parent.mkdir(parents=True, exist_ok=True)
        _RECORDER.save(str(out))
        console.print(f"[dim]saved cast: {out}[/dim]")


if __name__ == "__main__":
    main()
