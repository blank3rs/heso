#!/usr/bin/env python3
"""
heso agent demo.

Per AGENTS.md and skills/heso/SKILL.md, the agent is NOT inside heso —
heso is the tool, the LLM harness is the agent. The "agent we made"
is the heso skill loaded by Claude Code (or any harness that consumes
SKILL.md). This script wires that up for a recorded demo:

    1. Spawn `claude -p --output-format stream-json` in the repo,
       so Claude Code auto-discovers skills/heso/SKILL.md.
    2. Pipe a query in.
    3. Stream the events back, render them with rich.
    4. Save the session to an SVG for the README.

No anthropic SDK import. No second agent loop. The exact same Claude
Code your editor uses, driving heso via its own skill.

Fallback: with --no-claude (or when `claude` isn't on PATH), runs a
pure-heso pipeline (search -> batch read -> summary) so the demo is
still useful and recordable without an LLM in the loop. Be honest in
the UI about which mode you're in.
"""

import argparse
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
from rich.table import Table
from rich.text import Text


REPO_ROOT = Path(__file__).resolve().parent.parent
HESO = REPO_ROOT / "target" / "release" / "heso.exe"
if not HESO.exists():
    HESO = REPO_ROOT / "target" / "release" / "heso"

_RECORD = os.environ.get("HESO_DEMO_RECORD") == "1"
console = Console(width=110, force_terminal=True, legacy_windows=False, record=_RECORD)


# ----------------------------------------------------------------------------
# Banner
# ----------------------------------------------------------------------------

def banner():
    title = Text()
    title.append("heso", style="bold white on dark_violet")
    title.append("  ", style="")
    title.append("agent demo", style="dim")
    sub = Text(
        "Claude Code drives heso via the existing skill. No new agent loop.",
        style="dim italic",
    )
    console.print(Panel(Group(title, sub), border_style="dark_violet", padding=(0, 2)))


def short(s, n=80):
    s = str(s)
    return s if len(s) <= n else s[: n - 1] + "..."


# ----------------------------------------------------------------------------
# Path A: drive Claude Code (the real agent)
# ----------------------------------------------------------------------------

def claude_on_path():
    return shutil.which("claude") is not None


def run_via_claude_code(query):
    """
    Spawn `claude -p` from the repo root so it auto-discovers
    skills/heso/SKILL.md. Stream JSONL events back and render them.
    """
    intro = Text(
        "Calling Claude Code (claude -p) from the repo root.\n"
        "It picks up skills/heso/SKILL.md and uses heso verbs as its tools.",
        style="dim italic",
    )
    console.print(Panel(intro, border_style="dim"))

    # Allow heso invocations the agent issues via Bash. The skill uses
    # bare `heso` (and we also accept the relative release path).
    # Pass the query via stdin to keep argv simple and avoid flag-list
    # confusion with --allowed-tools.
    cmd = [
        "claude",
        "-p",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
        "--allowed-tools",
        "Bash(heso:*),Bash(./target/release/heso:*),Bash(./target/release/heso.exe:*)",
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

    final_text = []
    current_tool = None
    tool_start_ts = 0.0

    spinner_live = Live(Spinner("dots", "thinking…", style="cyan"), console=console, refresh_per_second=8)
    spinner_live.start()
    try:
        for line in proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue

            etype = ev.get("type")
            if etype == "assistant" and isinstance(ev.get("message"), dict):
                for block in ev["message"].get("content", []):
                    btype = block.get("type")
                    if btype == "text" and block.get("text", "").strip():
                        spinner_live.stop()
                        console.print(Panel(Text(block["text"].strip(), style="white"), title="agent", border_style="cyan", padding=(0, 1)))
                        spinner_live = Live(Spinner("dots", "thinking…", style="cyan"), console=console, refresh_per_second=8)
                        spinner_live.start()
                        final_text.append(block["text"].strip())
                    elif btype == "tool_use":
                        spinner_live.stop()
                        name = block.get("name", "?")
                        inputs = block.get("input", {}) or {}
                        flat = " ".join(f"{k}={short(json.dumps(v), 40)}" for k, v in inputs.items())
                        console.print(Text(f"  ↳ {name} ", style="bold magenta") + Text(short(flat, 90), style="dim"))
                        current_tool = name
                        tool_start_ts = time.time()
                        spinner_live = Live(Spinner("dots", "running tool…", style="yellow"), console=console, refresh_per_second=8)
                        spinner_live.start()
            elif etype == "user" and isinstance(ev.get("message"), dict):
                # Tool result blocks come back as user-role messages.
                for block in ev["message"].get("content", []):
                    if block.get("type") == "tool_result":
                        spinner_live.stop()
                        elapsed = int((time.time() - tool_start_ts) * 1000) if tool_start_ts else 0
                        ok = not block.get("is_error", False)
                        mark = Text("    ok ", style="green") if ok else Text("    err ", style="red")
                        size = 0
                        content = block.get("content")
                        if isinstance(content, str):
                            size = len(content)
                        elif isinstance(content, list):
                            for c in content:
                                if isinstance(c, dict) and isinstance(c.get("text"), str):
                                    size += len(c["text"])
                        console.print(mark + Text(f"{current_tool} -> {size} bytes ({elapsed}ms)", style="dim"))
                        spinner_live = Live(Spinner("dots", "thinking…", style="cyan"), console=console, refresh_per_second=8)
                        spinner_live.start()
            elif etype == "result":
                pass  # handled by stream end

    finally:
        try:
            spinner_live.stop()
        except Exception:
            pass
        proc.wait(timeout=30)

    if proc.returncode != 0:
        err = proc.stderr.read() if proc.stderr else ""
        console.print(Panel(Text(f"claude -p exited {proc.returncode}\n{err}", style="red"), border_style="red"))
        return

    if final_text:
        last = final_text[-1].strip()
        console.print(Panel(Text(last, style="white"), title="answer", border_style="green", padding=(1, 2)))


# ----------------------------------------------------------------------------
# Path B: offline (no LLM) — just demonstrate the verbs
# ----------------------------------------------------------------------------

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
        return proc.returncode == 0, json.loads(out), err
    except json.JSONDecodeError:
        return proc.returncode == 0, out, err


def tool_call_panel(verb, args, status="running"):
    cmd = Text()
    cmd.append("$ ", style="dim")
    cmd.append("heso ", style="bold cyan")
    cmd.append(verb, style="bold")
    if args:
        cmd.append(" ", style="")
        cmd.append(" ".join(args), style="white")
    label = {
        "running": Spinner("dots", style="yellow"),
        "ok": Text("ok", style="bold green"),
        "err": Text("err", style="bold red"),
    }[status]
    return Group(cmd, label)


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

    with Live(tool_call_panel("search", [f'"{user_input}"', "--limit 5"], "running"), refresh_per_second=8, console=console) as live:
        start = time.time()
        ok, data, _ = run_heso(["search", user_input, "--limit", "5"])
        elapsed = int((time.time() - start) * 1000)
        live.update(tool_call_panel("search", [f'"{user_input}"', "--limit 5", f"({elapsed}ms)"], "ok" if ok else "err"))

    results = (data or {}).get("results", []) if isinstance(data, dict) else []
    if results:
        t = Table(show_header=False, box=None, pad_edge=False, padding=(0, 1))
        t.add_column(style="dim")
        t.add_column()
        for r in results[:5]:
            t.add_row(
                f"#{r['rank']}",
                Text(short(r["title"], 70), style="bold")
                + Text(f"\n   {r['url']}", style="cyan"),
            )
        console.print(t)

    urls = [r["url"] for r in results[:3]]
    if not urls:
        console.print(Panel(Text("no results", style="red"), border_style="red"))
        return

    with Live(tool_call_panel("batch read", [f"--parallel 2 {len(urls)} urls"], "running"), refresh_per_second=8, console=console) as live:
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
        live.update(tool_call_panel("batch read", [f"--parallel 2 {len(urls)} urls", f"({elapsed}ms)"], "ok"))

    batch_rows = []
    for line in (proc.stdout or "").splitlines():
        if not line.strip():
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        batch_rows.append(d)

    t = Table(show_header=False, box=None, pad_edge=False, padding=(0, 1))
    t.add_column(width=3)
    t.add_column()
    for r in batch_rows:
        ok = "error" not in r
        mark = Text("ok", style="green") if ok else Text("er", style="red")
        label = Text(short(r.get("title") or r.get("url", ""), 70))
        t.add_row(mark, label)
    console.print(t)

    bits = [Text("Pages read:", style="bold")]
    for r in batch_rows:
        if "error" in r:
            continue
        bits.append(Text(f"  - {short(r.get('title') or '', 70)}", style="white"))
        bits.append(Text(f"    {r.get('url','')}", style="cyan"))
    console.print(Panel(Group(*bits), title="answer", border_style="green", padding=(1, 2)))


# ----------------------------------------------------------------------------
# Entrypoint
# ----------------------------------------------------------------------------

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

    if cli_args.save_svg and console.record:
        out = Path(cli_args.save_svg)
        out.parent.mkdir(parents=True, exist_ok=True)
        console.save_svg(str(out), title="heso - agent demo")
        console.print(f"[dim]saved {out}[/dim]")


if __name__ == "__main__":
    main()
