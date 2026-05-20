#!/usr/bin/env python3
"""
demo/record.py - record a single-task agent demo as an asciinema cast.

What it does:
  1. Prints a fake shell prompt and types the command character-by-character.
  2. Spawns `claude -p` with the heso skill auto-discovered.
  3. Streams Claude Code's stdout verbatim with timing.
  4. Saves an asciicast v2 file you can replay or convert to GIF.

Why this shape:
  No Python UI wrapper. The recorded session looks like someone typing
  a command in a real terminal, and the output is exactly what Claude
  Code prints. Cleaner, less truncation, one focused task.

Convert to GIF:
  agg demo/demo.cast demo/demo.gif --theme monokai --font-size 14
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

if sys.stdout.encoding and sys.stdout.encoding.lower() != "utf-8":
    sys.stdout.reconfigure(encoding="utf-8")


REPO_ROOT = Path(__file__).resolve().parent.parent

# Visual prompt and small typing animation parameters
PROMPT = "\x1b[38;5;212m~/heso\x1b[0m \x1b[38;5;141m$\x1b[0m "
TYPING_DELAY = 0.040   # seconds between characters when "typing"
POST_COMMAND_PAUSE = 0.6   # before the model starts streaming
LINE_PACE_FLOOR = 0.012   # min seconds between captured output writes


def write_cast(path, events, width=110, height=36, start_ts=None):
    header = {
        "version": 2,
        "width": width,
        "height": height,
        "timestamp": int(start_ts or time.time()),
        "env": {"SHELL": "/bin/bash", "TERM": "xterm-256color"},
    }
    with open(path, "w", encoding="utf-8") as f:
        f.write(json.dumps(header) + "\n")
        for e in events:
            f.write(json.dumps(e) + "\n")


def append_event(events, t, data):
    events.append([round(t, 4), "o", data])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--query",
        required=True,
        help="The single-line question to ask the agent.",
    )
    parser.add_argument(
        "--cast",
        default="demo/demo.cast",
        help="Output asciicast path.",
    )
    parser.add_argument(
        "--width",
        type=int,
        default=110,
        help="Asciicast width.",
    )
    parser.add_argument(
        "--height",
        type=int,
        default=36,
        help="Asciicast height.",
    )
    parser.add_argument(
        "--no-claude",
        action="store_true",
        help="Skip Claude Code; record a pure-heso pipeline.",
    )
    args = parser.parse_args()

    if not shutil.which("claude") and not args.no_claude:
        print("claude not on PATH; falling back to --no-claude", file=sys.stderr)
        args.no_claude = True

    # The visible command we type. Same query, surrounded with quotes.
    if args.no_claude:
        visible_cmd = f'python demo/agent.py --no-claude --query "{args.query}"'
    else:
        visible_cmd = (
            f'claude -p --allowed-tools "Bash(heso:*)" '
            f'"{args.query}"'
        )

    events = []
    start = time.time()
    t = 0.0

    # 1. Empty prompt
    append_event(events, t, PROMPT)
    t += 0.5

    # 2. Type the command char-by-char (simulated)
    for ch in visible_cmd:
        append_event(events, t, ch)
        t += TYPING_DELAY

    # 3. Press Enter -> newline
    t += 0.25
    append_event(events, t, "\r\n")
    t += POST_COMMAND_PAUSE

    # 4. Spawn the real command and capture its output with timing.
    env = os.environ.copy()
    env["PATH"] = (
        str(REPO_ROOT / "target" / "release")
        + os.pathsep
        + env.get("PATH", "")
    )

    if args.no_claude:
        cmd = [
            sys.executable,
            str(REPO_ROOT / "demo" / "agent.py"),
            "--no-claude",
            "--query",
            args.query,
        ]
        stdin_in = None
        is_stream_json = False
    else:
        # stream-json so we can see each tool_use as it happens and
        # render it as a clean inline step before the final answer.
        cmd = [
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--allowed-tools",
            "Bash(heso:*),Bash(./target/release/heso:*),Bash(./target/release/heso.exe:*),Bash(.\\target\\release\\heso.exe:*),Bash(.\\target\\release\\heso:*)",
        ]
        stdin_in = subprocess.PIPE
        is_stream_json = True

    proc = subprocess.Popen(
        cmd,
        cwd=str(REPO_ROOT),
        stdin=stdin_in,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        text=False,
    )
    if stdin_in is not None and proc.stdin is not None:
        proc.stdin.write(args.query.encode("utf-8"))
        proc.stdin.close()

    real_start = time.time()

    def emit(text, lag=0.0):
        nonlocal t
        t = max(t + lag, t + LINE_PACE_FLOOR)
        sys.stdout.write(text)
        sys.stdout.flush()
        append_event(events, t, text)

    if not is_stream_json:
        while True:
            chunk = proc.stdout.read(64) if proc.stdout else b""
            if not chunk:
                if proc.poll() is not None:
                    break
                time.sleep(0.01)
                continue
            try:
                decoded = chunk.decode("utf-8")
            except UnicodeDecodeError:
                decoded = chunk.decode("utf-8", errors="replace")
            elapsed = time.time() - real_start
            t = max(t, t + max(0.0, elapsed))
            emit(decoded)
    else:
        # ---- stream-json: render each event as a clean line ----
        tool_starts = {}
        seen_final = False
        ANSI_DIM = "\x1b[38;5;245m"
        ANSI_CYAN = "\x1b[38;5;117m"
        ANSI_MAGENTA = "\x1b[38;5;213m"
        ANSI_GREEN = "\x1b[38;5;120m"
        ANSI_RED = "\x1b[38;5;203m"
        ANSI_RESET = "\x1b[0m"

        # Initial "thinking" hint so there's something on screen while
        # the model spins up.
        emit(f"{ANSI_DIM}thinking...{ANSI_RESET}\r\n", lag=0.4)

        buf = b""
        thinking_visible = True

        def clear_thinking():
            nonlocal thinking_visible
            if thinking_visible:
                # Clear the "thinking..." line we just printed.
                emit("\x1b[1A\x1b[2K")
                thinking_visible = False

        while True:
            chunk = proc.stdout.read(1) if proc.stdout else b""
            if not chunk:
                if proc.poll() is not None:
                    break
                time.sleep(0.01)
                continue
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                try:
                    ev = json.loads(line.decode("utf-8", errors="replace"))
                except json.JSONDecodeError:
                    continue
                etype = ev.get("type")
                if etype == "assistant" and isinstance(ev.get("message"), dict):
                    for block in ev["message"].get("content", []):
                        if block.get("type") == "tool_use":
                            clear_thinking()
                            name = block.get("name", "?")
                            inputs = block.get("input", {}) or {}
                            cmd_preview = inputs.get("command") or ""
                            if not cmd_preview and isinstance(inputs, dict):
                                cmd_preview = " ".join(f"{k}={v}" for k, v in inputs.items())
                            cmd_preview = (cmd_preview[:120] + "...") if len(cmd_preview) > 120 else cmd_preview
                            emit(
                                f"{ANSI_MAGENTA}-> {name}{ANSI_RESET} {ANSI_DIM}{cmd_preview}{ANSI_RESET}\r\n",
                                lag=0.2,
                            )
                            tool_starts[block.get("id")] = time.time()
                elif etype == "user" and isinstance(ev.get("message"), dict):
                    for block in ev["message"].get("content", []):
                        if block.get("type") == "tool_result":
                            tid = block.get("tool_use_id")
                            started = tool_starts.get(tid, time.time())
                            elapsed_ms = int((time.time() - started) * 1000)
                            ok = not block.get("is_error", False)
                            content = block.get("content")
                            size = 0
                            if isinstance(content, str):
                                size = len(content)
                            elif isinstance(content, list):
                                for c in content:
                                    if isinstance(c, dict) and isinstance(c.get("text"), str):
                                        size += len(c["text"])
                            mark = f"{ANSI_GREEN}ok{ANSI_RESET}" if ok else f"{ANSI_RED}err{ANSI_RESET}"
                            emit(
                                f"   {mark} {ANSI_DIM}{elapsed_ms}ms · {size} bytes{ANSI_RESET}\r\n",
                                lag=0.1,
                            )
                elif etype == "result":
                    clear_thinking()
                    text = ev.get("result", "").strip()
                    if text and not seen_final:
                        seen_final = True
                        # Visual separator + final answer.
                        emit("\r\n", lag=0.3)
                        # Type the final answer one short chunk at a time
                        # so it feels paced, not dumped.
                        for line in text.splitlines(keepends=True):
                            for piece in re.findall(r".{1,80}(?:\s+|$)", line) or [line]:
                                emit(piece, lag=0.04)

    proc.wait()
    t += 0.8
    append_event(events, t, "\r\n" + PROMPT)
    t += 1.0

    out_path = Path(args.cast)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    write_cast(out_path, events, width=args.width, height=args.height, start_ts=start)
    print(f"\n[saved {out_path} - {len(events)} events]", file=sys.stderr)


if __name__ == "__main__":
    main()
