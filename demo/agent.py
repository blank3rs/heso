#!/usr/bin/env python3
"""
heso agent demo — a small Claude agent that uses heso as its tools.

The agent has four tools: search, read, batch_read, click. It gets a
task from you, decides what to do, runs heso commands, and reports
back what it found.

Run:
    python demo/agent.py
    # then type something like: "find me three rust web scraping libraries"

Needs ANTHROPIC_API_KEY in env. If not set, runs in offline-demo mode
that does a scripted search-and-summarize so the recording still works.

Built to be screen-recorded for the README. Keeps the UI tight and
high-contrast so it reads well in a GIF.
"""

import argparse
import io
import json
import os
import subprocess
import sys
import time
from pathlib import Path

# Force UTF-8 on Windows so the rich UI renders Unicode arrows etc.
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
console = Console(
    width=110,
    force_terminal=True,
    legacy_windows=False,
    record=_RECORD,
)


def banner():
    title = Text()
    title.append("heso", style="bold white on dark_violet")
    title.append("  ", style="")
    title.append("agent demo", style="dim")
    sub = Text(
        "small Rust browser. tiny binary. fast. one tool per agent verb.",
        style="dim italic",
    )
    console.print(Panel(Group(title, sub), border_style="dark_violet", padding=(0, 2)))


def run_heso(args, timeout=60):
    """Invoke the heso binary, return (ok, parsed_json_or_text, raw_stderr)."""
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


def show_tool_call(verb, args, status="running"):
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


def short(s, n=80):
    s = str(s)
    return s if len(s) <= n else s[: n - 1] + "…"


# ----------------------------------------------------------------------------
# heso tool wrappers (the agent calls these)
# ----------------------------------------------------------------------------

def tool_search(query, limit=5):
    ok, data, err = run_heso(["search", query, "--limit", str(limit)])
    if not ok or not isinstance(data, dict):
        return {"error": err or "search failed"}
    return {
        "query": data.get("query"),
        "knowledge": data.get("knowledge"),
        "results": [
            {"rank": r["rank"], "title": r["title"], "url": r["url"], "snippet": r.get("snippet", "")}
            for r in (data.get("results") or [])
        ],
    }


def tool_read(url, complete=False):
    args = ["read", url]
    if complete:
        args.append("--complete")
    ok, data, err = run_heso(args, timeout=45)
    if not ok or not isinstance(data, dict):
        return {"error": err or "read failed"}
    text = (data.get("text") or "")[:3000]
    return {
        "url": data.get("url"),
        "title": data.get("title"),
        "text": text,
        "actions_count": len(data.get("actions") or []),
        "framework": data.get("framework"),
    }


def tool_batch_read(urls, parallel=2):
    args = ["batch", "read", "--parallel", str(parallel), *urls]
    if not HESO.exists():
        return {"error": "no heso binary"}
    proc = subprocess.run(
        [str(HESO), *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=120,
    )
    results = []
    for line in (proc.stdout or "").strip().splitlines():
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        if "error" in d:
            results.append({"url": d.get("url"), "ok": False, "error": d["error"]})
        else:
            results.append(
                {
                    "url": d.get("url"),
                    "ok": True,
                    "title": d.get("title"),
                    "text": (d.get("text") or "")[:1500],
                    "actions_count": len(d.get("actions") or []),
                }
            )
    return {"results": results}


# ----------------------------------------------------------------------------
# Tool registry for the Claude agent
# ----------------------------------------------------------------------------

TOOLS_FOR_API = [
    {
        "name": "heso_search",
        "description": "Search the web via DuckDuckGo + Wikipedia. Returns a list of results and an optional knowledge block.",
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "default": 5, "maximum": 10},
            },
            "required": ["query"],
        },
    },
    {
        "name": "heso_read",
        "description": "Fetch a URL, run its JavaScript, return title + visible text + action list. Use --complete for sites with lazy-loaded content.",
        "input_schema": {
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "complete": {"type": "boolean", "default": False},
            },
            "required": ["url"],
        },
    },
    {
        "name": "heso_batch_read",
        "description": "Read several URLs in parallel. Returns one entry per URL with title + text + actions.",
        "input_schema": {
            "type": "object",
            "properties": {
                "urls": {"type": "array", "items": {"type": "string"}},
                "parallel": {"type": "integer", "default": 2, "maximum": 4},
            },
            "required": ["urls"],
        },
    },
]


def dispatch_tool(name, params):
    if name == "heso_search":
        return tool_search(params["query"], params.get("limit", 5))
    if name == "heso_read":
        return tool_read(params["url"], params.get("complete", False))
    if name == "heso_batch_read":
        return tool_batch_read(params["urls"], params.get("parallel", 2))
    return {"error": f"unknown tool {name}"}


SYSTEM_PROMPT = """You are an agent with access to a small headless browser called `heso`. Use it to answer the user's request.

Tools available:
- heso_search(query, limit) — web search
- heso_read(url, complete) — fetch a page and get its content
- heso_batch_read(urls, parallel) — fetch several pages in parallel

Be quick. 2-4 tool calls is usually enough. Format the final answer as plain prose, no markdown headers, mention specific facts you saw. Don't speculate."""


def run_claude_agent(user_input, client, model="claude-sonnet-4-5"):
    messages = [{"role": "user", "content": user_input}]
    panels = []

    while True:
        resp = client.messages.create(
            model=model,
            max_tokens=2048,
            system=SYSTEM_PROMPT,
            tools=TOOLS_FOR_API,
            messages=messages,
        )

        # Stream the model's text + tool calls
        text_parts = []
        tool_uses = []
        for block in resp.content:
            if block.type == "text" and block.text.strip():
                text_parts.append(block.text)
            elif block.type == "tool_use":
                tool_uses.append(block)

        if text_parts:
            txt = "\n".join(text_parts).strip()
            console.print(Panel(Text(txt, style="white"), title="agent", border_style="cyan", padding=(0, 1)))

        if not tool_uses:
            return "\n".join(text_parts).strip() or "(no answer)"

        # Run each tool, show the call live
        tool_results = []
        for tu in tool_uses:
            args_pretty = json.dumps(tu.input, indent=None)[1:-1].replace(", ", " ")
            with Live(show_tool_call(tu.name.replace("heso_", ""), [short(args_pretty, 70)], "running"), refresh_per_second=8, console=console) as live:
                start = time.time()
                result = dispatch_tool(tu.name, tu.input)
                elapsed = int((time.time() - start) * 1000)
                live.update(show_tool_call(tu.name.replace("heso_", ""), [short(args_pretty, 70), f"({elapsed}ms)"], "ok" if "error" not in result else "err"))

            # Render a small summary of what came back
            summary = render_tool_result_summary(tu.name, result)
            if summary:
                console.print(summary)

            tool_results.append({"type": "tool_result", "tool_use_id": tu.id, "content": json.dumps(result)[:8000]})

        messages.append({"role": "assistant", "content": resp.content})
        messages.append({"role": "user", "content": tool_results})


def render_tool_result_summary(tool_name, result):
    if "error" in result:
        return Panel(Text(result["error"], style="red"), border_style="red", padding=(0, 1))
    if tool_name == "heso_search":
        rows = result.get("results", [])
        if not rows:
            return Text("  (no results)", style="dim")
        t = Table(show_header=False, box=None, pad_edge=False, padding=(0, 1))
        t.add_column(style="dim")
        t.add_column()
        for r in rows[:5]:
            t.add_row(f"#{r['rank']}", Text(short(r["title"], 70), style="bold") + Text(f"\n   {r['url']}", style="cyan"))
        return t
    if tool_name == "heso_read":
        bits = []
        if result.get("title"):
            bits.append(Text("  title:   ", style="dim") + Text(short(result["title"], 70), style="bold"))
        if result.get("framework"):
            bits.append(Text("  framework: ", style="dim") + Text(result["framework"]))
        bits.append(Text(f"  actions: {result.get('actions_count', 0)}", style="dim"))
        return Group(*bits)
    if tool_name == "heso_batch_read":
        rows = result.get("results", [])
        t = Table(show_header=False, box=None, pad_edge=False, padding=(0, 1))
        t.add_column(width=3)
        t.add_column()
        for r in rows:
            mark = Text("✓", style="green") if r.get("ok") else Text("✗", style="red")
            label = Text(short(r.get("title") or r.get("url", ""), 70))
            t.add_row(mark, label)
        return t
    return None


def offline_demo(user_input):
    """Run a real heso pipeline without an API key. Honest about being scripted."""
    console.print(Panel(Text("(offline demo - set ANTHROPIC_API_KEY for live agent)\n\nrunning a search -> batch-read -> summarize pipeline", style="dim italic"), border_style="dim"))
    with Live(show_tool_call("search", [f'"{user_input}"', "--limit 5"], "running"), refresh_per_second=8, console=console) as live:
        start = time.time()
        search = tool_search(user_input, 5)
        elapsed = int((time.time() - start) * 1000)
        live.update(show_tool_call("search", [f'"{user_input}"', "--limit 5", f"({elapsed}ms)"], "ok"))
    s = render_tool_result_summary("heso_search", search)
    if s:
        console.print(s)

    urls = [r["url"] for r in search.get("results", [])[:3]]
    if not urls:
        console.print(Panel(Text("no results to read", style="red"), border_style="red"))
        return

    with Live(show_tool_call("batch read", [f"--parallel 2 {len(urls)} urls"], "running"), refresh_per_second=8, console=console) as live:
        start = time.time()
        batch = tool_batch_read(urls, 2)
        elapsed = int((time.time() - start) * 1000)
        live.update(show_tool_call("batch read", [f"--parallel 2 {len(urls)} urls", f"({elapsed}ms)"], "ok"))
    s = render_tool_result_summary("heso_batch_read", batch)
    if s:
        console.print(s)

    bits = []
    if search.get("knowledge"):
        k = search["knowledge"]
        bits.append(Text(k.get("title", "knowledge"), style="bold"))
        bits.append(Text(k.get("summary", ""), style=""))
        bits.append(Text(""))
    bits.append(Text("Top pages read:", style="bold"))
    for r in batch.get("results", []):
        if r.get("ok") and r.get("title"):
            bits.append(Text(f"  • {short(r['title'], 70)}", style="white"))
            bits.append(Text(f"    {r['url']}", style="cyan"))
    console.print(Panel(Group(*bits), title="answer", border_style="green", padding=(1, 2)))


def main():
    banner()
    if not HESO.exists():
        console.print(Panel(Text(f"heso binary not found at {HESO}\nrun: cargo build --release -p heso-cli", style="red"), border_style="red"))
        sys.exit(1)

    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if api_key:
        try:
            import anthropic
            client = anthropic.Anthropic(api_key=api_key)
        except ImportError:
            console.print("[dim]anthropic SDK missing — pip install anthropic[/dim]")
            client = None
    else:
        client = None

    parser = argparse.ArgumentParser()
    parser.add_argument("--query", help="Skip the prompt; use this query")
    parser.add_argument("--save-svg", help="After the run, save a styled SVG of the session here")
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
    if client:
        final = run_claude_agent(user_input, client)
        elapsed = int(time.time() - start)
        console.print(Panel(Text(final, style="white"), title=f"answer · {elapsed}s", border_style="green", padding=(1, 2)))
    else:
        offline_demo(user_input)
        elapsed = int(time.time() - start)
        console.print(Text(f"done in {elapsed}s", style="dim"))

    if cli_args.save_svg and console.record:
        out = Path(cli_args.save_svg)
        out.parent.mkdir(parents=True, exist_ok=True)
        console.save_svg(str(out), title="heso - agent demo")
        console.print(f"[dim]saved {out}[/dim]")


if __name__ == "__main__":
    main()
