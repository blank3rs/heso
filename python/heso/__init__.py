"""heso — the agent-native web engine, as a Python library.

This module is a thin subprocess wrapper around the bundled ``heso``
binary. Every call here:

1. Builds an argv list from positional args + ``snake_case`` kwargs
   (snake_case keys are translated to ``--dashed`` CLI flags).
2. Locates the bundled binary via :func:`_find_binary` — preferring
   ``<this-package>/bin/heso(.exe)`` (populated at release time by
   the release script / CI), falling back to ``heso`` on ``PATH``.
3. Spawns the binary with the assembled argv, captures stdout, and
   parses it as JSON.
4. Returns the parsed value as a native ``dict`` / ``list``, or raises
   :class:`HesoError` on a non-zero exit.

The contract is intentionally narrow: **no FFI, no Rust extension
module, no Python bindings to internal types**. Just subprocess and
JSON. The same binary you'd invoke from a shell prompt is the one
this library spawns; ``heso open URL`` from a terminal returns the
same JSON ``heso.open(URL)`` returns as a ``dict``.

Quick usage::

    import heso
    page = heso.open("https://example.com")          # -> dict
    results = heso.search("rust", limit=5)           # -> dict
    content = heso.read("https://example.com",
                        complete=True)               # -> dict

For multi-step flows that need to share cookies / DOM / JS state
across calls, use :class:`Session`, which is backed by a single
long-running ``heso serve`` JSON-RPC subprocess::

    with heso.session() as s:
        s.open("https://example.com")
        s.click(text="More information...")
        page = s.read()

The CLI is unchanged: after ``pip install heso``, ``heso open URL``
still works on ``PATH`` — the native binary is installed there
directly, so it runs without a Python interpreter in the hot path.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import threading
from itertools import count
from pathlib import Path
from typing import Any, Iterable, Mapping, Optional, Sequence, Union

__all__ = [
    "HesoError",
    "Session",
    "session",
    "open",
    "read",
    "wait",
    "search",
    "click",
    "fill",
    "submit",
    "eval_js",
    "eval_dom",
    "batch",
    "meta",
    "ls",
    "cat",
    "find",
    "tree",
    "stamp",
    "replay",
    "run_plat",
    "refresh",
    "verify",
    "info",
    "seal",
    "unseal",
    "identity",
    "run",
]


# ---------------------------------------------------------------------------
# Errors
# ---------------------------------------------------------------------------


class HesoError(Exception):
    """Raised when the ``heso`` binary exits non-zero, its stdout
    doesn't parse as JSON, or a ``heso serve`` JSON-RPC call returns
    an error envelope.

    Attributes:
        stdout: Captured stdout (str). May be empty.
        stderr: Captured stderr (str). May contain a human error line.
        returncode: Subprocess exit code from the binary. ``2`` means a
            usage / argument error (matches the CLI's convention); ``1``
            means a runtime failure; ``None`` when the error came from
            a session (JSON-RPC) call or we couldn't even spawn.
        rpc_code: JSON-RPC error code (e.g. ``-32601``) when the error
            came from a ``Session`` call. ``None`` for subprocess errors.
            Kept separate from ``returncode`` so callers branching on
            ``if e.returncode == 2`` don't misfire on wire-level errors.
        command: The full argv list we spawned, for debugging.
    """

    def __init__(
        self,
        message: str,
        *,
        stdout: str = "",
        stderr: str = "",
        returncode: Optional[int] = None,
        rpc_code: Optional[int] = None,
        command: Optional[Sequence[str]] = None,
    ) -> None:
        super().__init__(message)
        self.stdout = stdout
        self.stderr = stderr
        self.returncode = returncode
        self.rpc_code = rpc_code
        self.command = list(command) if command is not None else []


# ---------------------------------------------------------------------------
# Binary resolution
# ---------------------------------------------------------------------------


def _find_binary() -> str:
    """Locate the ``heso`` binary.

    Resolution order:

    1. The bundled binary at ``<package>/bin/heso{.exe}`` (populated
       by the release script / CI before the wheel is built).
    2. ``heso`` (or ``heso.exe`` on Windows) on ``PATH``.

    Returns the absolute path string. Raises :class:`HesoError` if
    neither is found.
    """
    exe = "heso.exe" if os.name == "nt" else "heso"
    bundled = Path(__file__).resolve().parent / "bin" / exe
    if bundled.is_file():
        return str(bundled)
    on_path = shutil.which("heso")
    if on_path:
        return on_path
    raise HesoError(
        "heso binary not found. Looked for a bundled copy at "
        f"{bundled} and for `heso` on PATH. Reinstall the package "
        "or download a release binary from "
        "https://github.com/blank3rs/heso/releases."
    )


# ---------------------------------------------------------------------------
# Binary / wrapper version handshake
# ---------------------------------------------------------------------------

_version_check_done = False


def _check_binary_version(binary_path: str) -> None:
    """Compare the wrapper's declared ``__version__`` against ``heso
    --version`` once per process. On mismatch, emit a single stderr
    warning. Skip entirely when ``HESO_SKIP_VERSION_CHECK=1`` is set.
    """
    global _version_check_done
    if _version_check_done:
        return
    _version_check_done = True
    if os.environ.get("HESO_SKIP_VERSION_CHECK") == "1":
        return
    try:
        proc = subprocess.run(
            [binary_path, "--version"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=5,
            check=False,
        )
    except Exception:
        return
    if proc.returncode != 0:
        return
    line = (proc.stdout or "").strip().splitlines()[0:1]
    if not line:
        return
    # Banner shape: "heso 0.1.4". Take the second whitespace-split token.
    parts = line[0].split()
    if len(parts) < 2:
        return
    binary_version = parts[1]
    if binary_version != __version__:
        sys.stderr.write(
            f"warning: heso wrapper version {__version__} found heso binary "
            f"version {binary_version} at {binary_path}; behavior may differ. "
            f"Set HESO_SKIP_VERSION_CHECK=1 to silence.\n"
        )


# ---------------------------------------------------------------------------
# argv assembly
# ---------------------------------------------------------------------------


def _flag_name(key: str) -> str:
    """Translate a Python ``snake_case`` kwarg key into a ``--dashed``
    CLI flag.

    ``selector_exists`` -> ``--selector-exists``
    ``js_fetch`` -> ``--js-fetch``
    """
    return "--" + key.replace("_", "-")


def _normalize_value(value: Any) -> Optional[str]:
    """Serialize one kwarg value for use as a CLI argument.

    - ``bool`` True becomes no value (the flag is emitted on its own,
      e.g. ``--complete``); ``False`` returns ``None`` to signal "skip".
    - ``None`` returns ``None`` to signal "skip".
    - ``dict`` / ``list`` are JSON-encoded.
    - Everything else is ``str(value)``.
    """
    if value is None or value is False:
        return None
    if value is True:
        return ""  # marker for "emit the flag with no value"
    if isinstance(value, (dict, list)):
        return json.dumps(value, separators=(",", ":"))
    return str(value)


_SPAWN_LEVEL_KEYS = frozenset({"binary"})


def _split_spawn_opts(kwargs: Mapping[str, Any]) -> tuple[dict, dict]:
    """Split ``kwargs`` into ``(spawn_opts, cli_kwargs)``.

    ``timeout`` straddles both layers by design. The user supplies
    seconds (the Python convention); we forward it to the CLI as
    ``--timeout <ms>`` so the binary's in-band timeout produces the
    structured ``{ok: false, error: {code: "timeout", ...}}`` envelope,
    and we install ``timeout + 5`` seconds as the ``subprocess.run``
    backstop in case the binary itself hangs. ``timeout=0`` and
    ``timeout=None`` opt out of both layers — the CLI receives
    ``--timeout 0`` and ``subprocess.run`` waits forever.

    ``binary`` is removed entirely from the CLI-flag stream and
    forwarded only as a spawn-level argument.
    """
    spawn: dict = {}
    cli = dict(kwargs)
    if "binary" in cli:
        spawn["binary"] = cli.pop("binary")
    if "timeout" in cli:
        raw = cli.pop("timeout")
        if raw is not None:
            try:
                seconds = float(raw)
            except (TypeError, ValueError):
                raise HesoError(f"timeout: expected a number of seconds, got {raw!r}")
            ms = int(round(seconds * 1000))
            # Forward to the CLI so the binary's per-request budget
            # fires first and emits the structured envelope.
            cli["timeout"] = f"{ms}ms"
            # subprocess.run treats `timeout=0` as immediate kill;
            # preserve "no timeout" by skipping the spawn-level cap.
            if seconds > 0:
                spawn["timeout"] = seconds + 5.0
    return spawn, cli


def _kwargs_to_argv(kwargs: Mapping[str, Any]) -> list[str]:
    """Convert ``**kwargs`` to a flat argv list of CLI flags.

    ``--field`` is special: it accepts repeated ``NAME=VALUE`` pairs
    and may be passed as a ``dict`` or ``list[tuple[str, str]]``.
    ``--inject-script`` is repeatable and may be passed as one string
    or a list of strings.
    Everything else: one kwarg -> one flag (with or without a value).
    """
    argv: list[str] = []
    for key, value in kwargs.items():
        if key in _SPAWN_LEVEL_KEYS:
            continue
        flag = _flag_name(key)

        # `--field` is the lone CLI flag that legitimately repeats.
        # Accept dict / iterable[(k, v)] / iterable[str].
        if key == "field":
            if isinstance(value, Mapping):
                for k, v in value.items():
                    argv.extend([flag, f"{k}={v}"])
            elif isinstance(value, (list, tuple)):
                for item in value:
                    if isinstance(item, str):
                        argv.extend([flag, item])
                    elif isinstance(item, (list, tuple)) and len(item) == 2:
                        argv.extend([flag, f"{item[0]}={item[1]}"])
                    else:
                        raise HesoError(
                            f"field entries must be 'name=value' strings or "
                            f"(name, value) pairs; got {item!r}"
                        )
            elif value is None or value is False:
                continue
            else:
                raise HesoError(
                    f"`field=` must be a dict or list of pairs; got {type(value).__name__}"
                )
            continue

        if key == "inject_script" and isinstance(value, (list, tuple)):
            for item in value:
                normalized = _normalize_value(item)
                if normalized is None:
                    continue
                if normalized == "":
                    raise HesoError("inject_script entries must be strings, not bools")
                argv.extend([flag, normalized])
            continue

        normalized = _normalize_value(value)
        if normalized is None:
            continue
        if normalized == "":
            argv.append(flag)  # bool True -> bare flag
        else:
            argv.extend([flag, normalized])
    return argv


# ---------------------------------------------------------------------------
# Core spawn-and-parse
# ---------------------------------------------------------------------------


def run(
    *args: str,
    timeout: Optional[float] = None,
    parse_json: bool = True,
    binary: Optional[str] = None,
) -> Any:
    """Spawn the heso binary with ``args`` and return parsed stdout.

    This is the low-level escape hatch. The typed verbs (:func:`open`,
    :func:`read`, …) all funnel through here. Use it directly to call
    a CLI subcommand the wrapper doesn't expose yet.

    Args:
        *args: Positional argv to pass to ``heso``. The binary path
            is prepended for you; do NOT include ``"heso"`` here.
        timeout: Wall-clock timeout in seconds, forwarded to
            :class:`subprocess.Popen`. ``None`` means wait forever.
        parse_json: If ``True`` (default), the captured stdout is
            parsed as JSON before returning. Set ``False`` for verbs
            whose output isn't JSON (e.g. ``batch`` emits JSON-Lines).
        binary: Override the binary path resolution. Mostly for
            testing.

    Returns:
        The parsed JSON value (any of ``dict``, ``list``, ``str``,
        ``int``, ``float``, ``bool``, ``None``), or the raw stdout
        string if ``parse_json=False``.

    Raises:
        HesoError: on non-zero exit, JSON parse failure, or spawn
            failure (with ``stdout`` / ``stderr`` / ``returncode``
            / ``command`` attached).
    """
    exe = binary or _find_binary()
    _check_binary_version(exe)
    command = [exe, *args]
    try:
        # `text=True` decodes stdout/stderr as text using the locale
        # encoding. heso emits UTF-8 JSON on stdout so this works on
        # both Windows (cp1252-ish default) and POSIX.
        proc = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            # `check=False`: we read the return code ourselves so we
            # can wrap a useful error.
            check=False,
        )
    except FileNotFoundError as e:
        raise HesoError(
            f"failed to spawn {exe}: {e}",
            command=command,
        ) from e
    except subprocess.TimeoutExpired as e:
        raise HesoError(
            f"heso timed out after {timeout}s",
            stdout=(e.stdout or "") if isinstance(e.stdout, str) else "",
            stderr=(e.stderr or "") if isinstance(e.stderr, str) else "",
            command=command,
        ) from e

    stdout = proc.stdout or ""
    stderr = proc.stderr or ""

    if proc.returncode != 0:
        msg = stderr.strip() or f"heso exited with code {proc.returncode}"
        raise HesoError(
            msg,
            stdout=stdout,
            stderr=stderr,
            returncode=proc.returncode,
            command=command,
        )

    if not parse_json:
        return stdout

    try:
        return json.loads(stdout)
    except json.JSONDecodeError as e:
        raise HesoError(
            f"heso stdout did not parse as JSON: {e}",
            stdout=stdout,
            stderr=stderr,
            returncode=proc.returncode,
            command=command,
        ) from e


# ---------------------------------------------------------------------------
# Typed verbs (one function per heso subcommand)
# ---------------------------------------------------------------------------


def open(url: str, **kwargs: Any) -> dict:
    """``heso open <url>`` — fetch a page and return the agent-shaped
    summary as a dict.

    Returns ``{url, title, description, metadata, tree, actions,
    plat_hash, ...}``. With ``explore_links=N`` (>=1) also includes
    ``linked_pages`` with same-origin links pre-fetched.

    Kwargs are translated to CLI flags: ``explore_links=2`` ->
    ``--explore-links 2``, ``link_cap=10`` -> ``--link-cap 10``,
    ``best_effort=True`` -> ``--best-effort``,
    ``inject_script=["window.X=1"]`` -> repeat ``--inject-script`` (or
    pass once as a string).

    ``timeout`` (float seconds, default 30) becomes both the CLI's
    per-request ``--timeout`` budget and a ``subprocess.run`` kill
    backstop. On in-band timeout the binary exits 1 with a structured
    ``{ok: false, error: {code: "timeout", ...}}`` envelope on stdout,
    surfaced via :class:`HesoError`'s ``stdout`` attribute.

    ``no_private_networks=True`` refuses the request when the target
    resolves to a private / loopback / link-local / metadata IP (SSRF
    protection; off by default, global across every network verb).
    """
    spawn, cli = _split_spawn_opts(kwargs)
    return run("open", url, *_kwargs_to_argv(cli), **spawn)


def read(url: str, **kwargs: Any) -> dict:
    """``heso read <url>`` — fetch, run JS, and return the full picture.

    Returns ``{title, text, tree, actions, forms, cookies, console,
    framework, content_hash, ...}``.

    Common kwargs:
        complete: bool — auto-scroll loop until DOM settles.
        include: str — comma-separated subset of
            ``text,forms,cookies,console,framework,scripts``.
        js_fetch: bool — install the JS fetch() global.
        since: str — prior ``content_hash`` for diffing.
        best_effort: bool — exit 0 on partial failures.
        no_private_networks: bool — refuse private/loopback/metadata
            IPs (SSRF protection; off by default, global).
        timeout: float — per-request budget in seconds (default 30).
    """
    spawn, cli = _split_spawn_opts(kwargs)
    return run("read", url, *_kwargs_to_argv(cli), **spawn)


def wait(url: str, **kwargs: Any) -> dict:
    """``heso wait <url>`` — block until a page condition is satisfied.

    Returns ``{ok, elapsed_ms, condition, ...}``. Exit 1 on timeout
    (raised as :class:`HesoError`); exit 0 on satisfied.

    Common kwargs:
        selector_exists: str — CSS selector to wait for.
        text_contains: str — substring to wait for in ``document.body.textContent``.
        url_matches: str — regex against ``window.location.href``.
        network_idle: bool — no queued fetch/timer for ``idle_window``.
        idle_window: str — duration like ``"500ms"``.
        time: str — advance virtual clock, e.g. ``"2s"``.
        timeout: float — overall cap in seconds (default 30).
    """
    spawn, cli = _split_spawn_opts(kwargs)
    return run("wait", url, *_kwargs_to_argv(cli), **spawn)


def search(query: str, **kwargs: Any) -> dict:
    """``heso search <query>`` — web search across Mojeek, DuckDuckGo, and
    Wikipedia (optional SearXNG). No API key.

    Returns ``{query, engines_used, results, knowledge}`` as a dict:
    ``results`` is a list of ``{rank, title, url, snippet, source}`` rows;
    ``knowledge`` is a single ``{title, summary, url}`` block from
    Wikipedia (or ``None`` when there's no direct match).

    Common kwargs:
        limit: int — cap on results (default 30, max 100).
        engines: str — comma-separated subset of ``ddg,mojeek,wiki,searxng``
            (default ``ddg,mojeek,wiki``).
        searx_url: str — base URL for a SearXNG instance. Also reads
            ``HESO_SEARX_URL`` from the environment.
    """
    return run("search", query, *_kwargs_to_argv(kwargs))


def click(url: str, ref: Optional[str] = None, **kwargs: Any) -> dict:
    """``heso click <url> [<@ref> | --text S | --selector CSS | --aria-label S]``.

    Pass either a positional ``ref`` (e.g. ``"@e7"``) OR a locator
    kwarg: ``text="Sign in"``, ``selector="button.cta"``, or
    ``aria_label="Close"``.

    Returns the unified writing-verb envelope::

        {
          "ok": True | False,
          "op": "click",
          "url": ...,
          "ref": "@eN",
          "selector": "<resolved CSS selector>",
          "element_id": "<id attr>" | None,
          "value": None,             # click doesn't take a string
          "result": {...},           # engine-specific payload
          "console": [...]
        }

    A selector miss surfaces as ``ok: False`` with
    ``error: {"code": "selector_not_matched", "message": ...}``.

    A non-navigating click (an in-page handler that mutates the DOM
    rather than following a link) additionally carries ``text``,
    ``tree``, and ``content_hash`` — a post-click DOM snapshot, the
    same shape :func:`read` returns.

    Navigation fields: ``url`` is the page where the click happened
    (post any redirects on that page's own fetch); ``final_url`` is
    where navigation actually landed after following an ``<a href>``
    plus its own redirect chain (equals ``url`` for non-navigating
    clicks); and ``redirects`` is the ``{from, to, status}`` hops the
    navigation walked through, empty for direct hits and for clicks
    that did not navigate.

    Accepts ``timeout=N`` (seconds, default 30) capping the underlying
    HTTP request.
    """
    spawn, cli = _split_spawn_opts(kwargs)
    extra = _kwargs_to_argv(cli)
    if ref is not None:
        return run("click", url, ref, *extra, **spawn)
    return run("click", url, *extra, **spawn)


def fill(
    url: str,
    ref_or_value: str,
    value: Optional[str] = None,
    **kwargs: Any,
) -> dict:
    """``heso fill <url> (<@ref> | --text S | ...) <value>``.

    Two call shapes:

    - ``heso.fill(url, "@e3", "hello")`` — positional ref + value.
    - ``heso.fill(url, "hello", text="Email")`` — value first when
      using a locator kwarg.

    Returns the unified writing-verb envelope with ``value`` carrying
    the exact string passed in (the typed bytes), not a success flag.
    When the selector misses, ``ok`` is ``False`` and ``value`` still
    reflects the requested string so the caller can retry with a
    different locator.

    Accepts ``timeout=N`` (seconds, default 30) capping the underlying
    HTTP request.
    """
    spawn, cli = _split_spawn_opts(kwargs)
    extra = _kwargs_to_argv(cli)
    if value is not None:
        return run("fill", url, ref_or_value, value, *extra, **spawn)
    # Locator via kwargs; the single positional is the value.
    return run("fill", url, *extra, ref_or_value, **spawn)


def submit(url: str, ref: Optional[str] = None, **kwargs: Any) -> dict:
    """``heso submit <url> (<@form-ref> | --text S | ...) [--field N=V] [--data JSON]``.

    Kwargs:
        field: dict | list of pairs — repeated ``NAME=VALUE`` flags.
        data: dict — alternative ``--data`` JSON dict; ``field``
            wins on key collisions (matches the CLI).
        timeout: float — per-request budget in seconds (default 30).

    Returns ``{ok, op: "submit", url, ref, selector, element_id,
    value: None, result, console, postUrl}``. ``value`` is ``None`` for
    submit; the structured form-submission outcome (``matched``,
    ``submitted``, ``responseStatus``, ``responseJson``,
    ``fieldsApplied``, ...) lives under ``result``. ``postUrl`` is the
    response URL after redirects.
    """
    spawn, cli = _split_spawn_opts(kwargs)
    extra = _kwargs_to_argv(cli)
    if ref is not None:
        return run("submit", url, ref, *extra, **spawn)
    return run("submit", url, *extra, **spawn)


def eval_js(js: str, **kwargs: Any) -> dict:
    """``heso eval-js <js>`` — evaluate JS in a sandboxed QuickJS context.

    Returns ``{value, console, ...}``. ``seed=N`` seeds the
    determinism shims. ``js_timeout="5s"`` caps script wall-clock and
    returns a structured ``timeout`` error on expiry (default: no cap).
    No DOM — use :func:`eval_dom` for that.
    """
    spawn, cli = _split_spawn_opts(kwargs)
    return run("eval-js", *_kwargs_to_argv(cli), js, **spawn)


def eval_dom(url: str, js: str, **kwargs: Any) -> dict:
    """``heso eval-dom <url> <js>`` — fetch, run page scripts, then
    eval ``js`` against the post-hydration DOM.

    Returns ``{ok, url, value, console, ...}``.

    Common kwargs:
        seed: int — RNG seed (default 0).
        js_fetch: bool — install the JS fetch() global.
        js_timeout: str — cap script wall-clock (e.g. "5s"); returns a
            structured timeout error on expiry (default: no cap).
        timeout: float — per-request budget in seconds (default 30).
    """
    spawn, cli = _split_spawn_opts(kwargs)
    return run("eval-dom", *_kwargs_to_argv(cli), url, js, **spawn)


def batch(
    subverb: str,
    urls: Iterable[str],
    **kwargs: Any,
) -> list[dict]:
    """``heso batch [open|read] <urls...>`` — parallel multi-URL scrape.

    Unlike the other verbs, ``batch`` emits JSON-Lines on stdout (one
    JSON object per URL, completion-ordered). This wrapper splits
    those into a Python ``list[dict]``.

    Common kwargs:
        parallel: int — concurrent slots (default 8 for open, 2 for read).
        timeout_per_url: str — per-URL cap, e.g. ``"5s"`` (alias of ``timeout``).
        fail_fast: bool — stop on first error.
        include: str — passed through to the read subverb.
        js_fetch: bool — passed through to the read subverb.
        timeout: float — per-URL cap in seconds (alias of ``timeout_per_url``).
    """
    url_list = list(urls)
    spawn, cli = _split_spawn_opts(kwargs)
    raw = run(
        "batch",
        subverb,
        *_kwargs_to_argv(cli),
        *url_list,
        parse_json=False,
        **spawn,
    )
    out: list[dict] = []
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            # The CLI sometimes emits a non-JSON banner / progress
            # line; skip it instead of failing the whole batch.
            continue
    return out


def meta(url: str, **kwargs: Any) -> dict:
    """``heso meta <url>`` — extract structured metadata (JSON-LD,
    OpenGraph, SEO meta, canonical, icons, lang). Accepts ``timeout=N``
    (seconds, default 30) capping the underlying HTTP request."""
    spawn, cli = _split_spawn_opts(kwargs)
    return run("meta", url, *_kwargs_to_argv(cli), **spawn)


def ls(url: str, path: str = "/", **kwargs: Any) -> dict:
    """``heso ls <url> [path]`` — list children of a tree path. Accepts
    ``timeout=N`` (seconds, default 30) capping the underlying HTTP request."""
    spawn, cli = _split_spawn_opts(kwargs)
    return run("ls", url, path, *_kwargs_to_argv(cli), **spawn)


def cat(url: str, target: str, **kwargs: Any) -> dict:
    """``heso cat <url> <path|@ref>`` — read a tree path's text or an
    element ref's full record. Accepts ``timeout=N`` (seconds, default 30)
    capping the underlying HTTP request."""
    spawn, cli = _split_spawn_opts(kwargs)
    return run("cat", url, target, *_kwargs_to_argv(cli), **spawn)


def find(url: str, **kwargs: Any) -> dict:
    """``heso find <url>`` — list interactive elements (action graph)
    with optional filters.

    Kwargs:
        role: str — filter by ARIA role.
        name: str — filter by name substring.
        section: str — filter by section path, e.g. ``"/pricing"``.
        timeout: float — per-request budget in seconds (default 30).
    """
    spawn, cli = _split_spawn_opts(kwargs)
    return run("find", url, *_kwargs_to_argv(cli), **spawn)


def tree(url: str, **kwargs: Any) -> dict:
    """``heso tree <url>`` — full heading-derived page tree as JSON.
    Accepts ``timeout=N`` (seconds, default 30) capping the underlying
    HTTP request."""
    spawn, cli = _split_spawn_opts(kwargs)
    return run("tree", url, *_kwargs_to_argv(cli), **spawn)


def stamp(path: Union[str, Path], **kwargs: Any) -> dict:
    """``heso stamp <plan-or-plat>`` — execute a plan against the
    live web and mint a fresh plat that embeds the plan.

    Accepts a bare ``Action[]`` JSON array, a plat with a ``"plan"``
    field, or a ``TraceFingerprint``. Returns the stamped plat as a
    ``dict``. On a partial run the returned plat carries ``error`` and
    ``steps`` fields documenting which step failed, and ``run`` raises
    :class:`HesoError` (with the partial plat still on ``stdout``).

    Common kwargs:
        seed: int — RNG seed (default 0).
        template: str — load a v0 plan template from disk.
        values: dict — substitution map for ``{{name}}`` placeholders
            in the template.
        timeout: float — per-network-step budget in seconds (default 30).

    Keyword arguments become CLI flags via the same rules as every
    other verb.
    """
    spawn, cli = _split_spawn_opts(kwargs)
    return run("stamp", str(path), *_kwargs_to_argv(cli), **spawn)


def replay(path: Union[str, Path], **kwargs: Any) -> dict:
    """``heso replay <plat.plat>`` — read the recorded step log from a
    plat. **Does not** execute the engine, touch the network, or produce
    a fresh plat — use :func:`run` for low-level calls or the CLI
    ``heso run`` verb when you want cassette-backed re-execution.

    Returns a dict shaped ``{steps_count, plat_hash, cassette_records,
    steps}``. Pass ``plan=True`` to return the plat's ``plan`` field
    instead, for editing.
    """
    spawn, cli = _split_spawn_opts(kwargs)
    return run("replay", str(path), *_kwargs_to_argv(cli), **spawn)


def run_plat(path: Union[str, Path], **kwargs: Any) -> dict:
    """``heso run <plat.plat>`` — re-execute a stamped plat's plan
    against its embedded cassette. No network — cassette misses surface
    as structured errors per HESO/1.0 §5.5.

    Returns a fresh plat dict over the post-run state. For an unmodified
    cassette the returned ``plat_hash`` is byte-identical to the
    input's. Use :func:`replay` for the no-engine inspector that just
    emits the recorded step log, and :func:`stamp` to mint a plat
    against the live web.

    Named ``run_plat`` to avoid shadowing the low-level :func:`run`
    escape hatch.

    By default the input plat's integrity is verified before replay;
    ``no_verify_input=True`` forwards ``--no-verify-input`` to skip
    that check.

    Keyword arguments (e.g. ``seed=42``) become CLI flags.
    """
    spawn, cli = _split_spawn_opts(kwargs)
    return run("run", str(path), *_kwargs_to_argv(cli), **spawn)


def refresh(path: Union[str, Path], **kwargs: Any) -> dict:
    """``heso refresh <plat>`` — re-stamp a plat against the live web
    and return drift status.

    Returns ``{"ok": True, "drifted": bool, "input_plat_hash": str,
    "live_plat_hash": str, "diff": {...}}``. The wrapper resolves
    regardless of drift status (exit 0 vs 1); raises :class:`HesoError`
    only on usage errors (exit 2 — missing plan field, unreachable site).

    Accepts ``timeout=N`` (seconds, default 30) capping each per-step
    HTTP request the re-stamp makes.
    """
    spawn, cli = _split_spawn_opts(kwargs)
    try:
        return run("refresh", str(path), *_kwargs_to_argv(cli), **spawn)
    except HesoError as e:
        if e.returncode == 1 and e.stdout:
            try:
                return json.loads(e.stdout)
            except ValueError:
                pass
        raise


# ---------------------------------------------------------------------------
# Polymorphic verbs
# ---------------------------------------------------------------------------


def verify(path: Union[str, Path], **kwargs: Any) -> dict:
    """``heso verify <file>`` — verify integrity and/or signature of a plat, receipt, or sealed envelope."""
    spawn, cli = _split_spawn_opts(kwargs)
    return run("verify", str(path), *_kwargs_to_argv(cli), **spawn)


def info(
    path_or_paths: Union[str, Path, Sequence[Union[str, Path]]],
    **kwargs: Any,
) -> dict:
    """``heso info <file> [<file2>]`` — display plat metadata, or diff two plats.

    Pass a single path to inspect one plat. Pass a two-element sequence to diff.
    """
    spawn, cli = _split_spawn_opts(kwargs)
    if isinstance(path_or_paths, (str, Path)):
        return run("info", str(path_or_paths), *_kwargs_to_argv(cli), **spawn)
    paths = [str(p) for p in path_or_paths]
    return run("info", *paths, *_kwargs_to_argv(cli), **spawn)


def seal(path: Union[str, Path], **kwargs: Any) -> dict:
    """``heso seal <file> [--key PATH]`` — Ed25519 envelope."""
    spawn, cli = _split_spawn_opts(kwargs)
    return run("seal", str(path), *_kwargs_to_argv(cli), **spawn)


def unseal(path: Union[str, Path], **kwargs: Any) -> dict:
    """``heso unseal <file> [--extract]`` — verify a sealed envelope."""
    spawn, cli = _split_spawn_opts(kwargs)
    return run("unseal", str(path), *_kwargs_to_argv(cli), **spawn)


# ---------------------------------------------------------------------------
# Identity
# ---------------------------------------------------------------------------


def identity(subcommand: str, *args: str, **kwargs: Any) -> dict:
    """``heso identity <subcommand> [args]`` — Ed25519 key management.

    Today's subcommands are ``init`` (mint a fresh key) and ``show``
    (print the pubkey). Both accept ``--path PATH`` (default
    ``heso-local-data/identity.key``) and emit
    ``{path, public_key, algorithm}`` JSON, returned here as a dict.

    A single typed entry instead of one function per subcommand keeps
    the surface stable as new subcommands land.
    """
    if not isinstance(subcommand, str) or not subcommand:
        raise HesoError("identity: subcommand is required (e.g. 'init')")
    return run("identity", subcommand, *(str(a) for a in args), *_kwargs_to_argv(kwargs))


# ---------------------------------------------------------------------------
# Stateful session (wraps `heso serve` JSON-RPC)
# ---------------------------------------------------------------------------


class Session:
    """Long-lived ``heso serve`` JSON-RPC subprocess.

    Use this for flows that need to share cookies / DOM / JS state
    across calls (login, navigate, scrape; click sequences within an
    SPA). Each method maps to a JSON-RPC method on the server. See
    ``serve.rs`` for the wire format.

    Sessions are not thread-safe at the wire layer, but methods take
    a lock so concurrent callers serialize through the single stdin
    pipe.

    Recommended usage is the context manager::

        with heso.session() as s:
            s.open("https://example.com")
            s.click(text="More")
            page = s.read()

    If you can't use a ``with`` block, call :meth:`close` explicitly
    to terminate the subprocess.
    """

    def __init__(self, binary: Optional[str] = None) -> None:
        self._binary = binary or _find_binary()
        self._lock = threading.Lock()
        self._id_iter = count(1)
        self._proc: Optional[subprocess.Popen[str]] = None
        self._start()

    def _start(self) -> None:
        _check_binary_version(self._binary)
        # `bufsize=1` => line-buffered text mode, which is what
        # newline-delimited JSON-RPC wants. text=True picks up
        # universal-newlines so \r\n on Windows still splits cleanly.
        self._proc = subprocess.Popen(
            [self._binary, "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
        # Drain the `ready` notification the server emits on start.
        # It carries `method: "ready"`, no `id`; we just skip it.
        assert self._proc.stdout is not None
        line = self._proc.stdout.readline()
        if not line:
            stderr = ""
            if self._proc.stderr is not None:
                try:
                    stderr = self._proc.stderr.read()
                except Exception:
                    pass
            raise HesoError(
                "heso serve exited before emitting the ready notification",
                stderr=stderr,
                returncode=self._proc.returncode,
            )
        # Best-effort sanity check; don't fail hard if heso changes
        # the notification shape later.
        try:
            msg = json.loads(line)
            if msg.get("method") != "ready":
                # Could be an error or a real response — push it back
                # by raising so the user notices.
                raise HesoError(
                    f"expected 'ready' notification, got: {line.strip()}"
                )
        except json.JSONDecodeError as e:
            raise HesoError(
                f"first line from heso serve was not JSON: {line!r}"
            ) from e

    def _request(self, method: str, params: Optional[dict] = None) -> Any:
        if self._proc is None or self._proc.poll() is not None:
            raise HesoError("heso serve subprocess is not running")

        req_id = next(self._id_iter)
        payload = {
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params or {},
        }
        line = json.dumps(payload) + "\n"

        with self._lock:
            assert self._proc.stdin is not None
            assert self._proc.stdout is not None
            try:
                self._proc.stdin.write(line)
                self._proc.stdin.flush()
            except (BrokenPipeError, OSError) as e:
                raise HesoError(
                    f"failed to write to heso serve stdin: {e}",
                    returncode=self._proc.returncode,
                ) from e

            # Read responses until we see one with our request id.
            # Skipping notifications (no `id`) along the way.
            while True:
                response_line = self._proc.stdout.readline()
                if not response_line:
                    raise HesoError(
                        "heso serve closed stdout before responding",
                        returncode=self._proc.returncode,
                    )
                try:
                    resp = json.loads(response_line)
                except json.JSONDecodeError as e:
                    raise HesoError(
                        f"heso serve emitted non-JSON line: {response_line!r}"
                    ) from e
                # Skip stray notifications (e.g. a late `ready`).
                if "id" not in resp or resp["id"] is None:
                    continue
                if resp.get("id") != req_id:
                    # Out-of-order shouldn't happen in v1 (serve.rs
                    # comment: "strictly sequential"), but if it does
                    # we'd rather raise than silently consume the
                    # wrong response.
                    raise HesoError(
                        f"heso serve response id mismatch: "
                        f"expected {req_id}, got {resp.get('id')!r}"
                    )
                if "error" in resp and resp["error"] is not None:
                    err = resp["error"]
                    raise HesoError(
                        err.get("message", "unknown JSON-RPC error"),
                        rpc_code=err.get("code"),
                    )
                return resp.get("result")

    # ------- typed RPC methods ------------------------------------------

    def open(self, url: str, **kwargs: Any) -> dict:
        """RPC ``open`` — fetch a URL into a page cache slot."""
        return self._request("open", {"url": url, **kwargs})

    def read(self, **kwargs: Any) -> dict:
        """RPC ``read`` — return the read snapshot for ``page_id``
        (defaults to the most recent)."""
        return self._request("read", kwargs)

    def ls(self, path: str = "/", **kwargs: Any) -> dict:
        return self._request("ls", {"path": path, **kwargs})

    def cat(self, target: str, **kwargs: Any) -> dict:
        return self._request("cat", {"target": target, **kwargs})

    def find(self, **kwargs: Any) -> dict:
        return self._request("find", kwargs)

    def click(self, **kwargs: Any) -> dict:
        """RPC ``click`` — kwargs ``ref="@e7"`` or ``text=...`` etc."""
        return self._request("click", kwargs)

    def fill(self, value: str, **kwargs: Any) -> dict:
        return self._request("fill", {"value": value, **kwargs})

    def submit(self, **kwargs: Any) -> dict:
        return self._request("submit", kwargs)

    def eval(self, js: str, **kwargs: Any) -> dict:
        return self._request("eval", {"js": js, **kwargs})

    def navigate(self, url: str, **kwargs: Any) -> dict:
        return self._request("navigate", {"url": url, **kwargs})

    def wait(self, **kwargs: Any) -> dict:
        return self._request("wait", kwargs)

    def search(self, query: str, **kwargs: Any) -> dict:
        return self._request("search", {"query": query, **kwargs})

    def ping(self) -> Any:
        return self._request("ping")

    def close_page(self, page_id: str) -> dict:
        return self._request("close", {"page_id": page_id})

    # ------- lifecycle --------------------------------------------------

    def close(self) -> None:
        """Terminate the underlying ``heso serve`` subprocess."""
        if self._proc is None:
            return
        try:
            if self._proc.stdin is not None and not self._proc.stdin.closed:
                self._proc.stdin.close()
        except Exception:
            pass
        try:
            self._proc.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            try:
                self._proc.wait(timeout=2.0)
            except Exception:
                pass
        self._proc = None

    def __enter__(self) -> "Session":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass


def session(binary: Optional[str] = None) -> Session:
    """Construct a new :class:`Session`. Mostly cosmetic — sugar over
    ``heso.Session()`` so ``with heso.session() as s: ...`` reads
    naturally."""
    return Session(binary=binary)


# ---------------------------------------------------------------------------
# Version
# ---------------------------------------------------------------------------

# Kept in sync at release time by `.github/workflows/pypi.yml` (its
# "Set Python package __version__ from tag" step rewrites this line).
# The value here is the same default the workspace ships with.
__version__ = "0.1.9"
