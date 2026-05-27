"""``heso.registry`` — public plat registry operations.

Each function maps to ``heso registry <subcommand>`` and shares the
subprocess-wrapper plumbing in :mod:`heso`. Importable both as the
submodule (``from heso import registry; registry.publish(...)``) and
as module-level names (``from heso.registry import publish``).
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Iterable, Optional, Union

from . import HesoError, _kwargs_to_argv, run

__all__ = ["publish", "pull", "list", "search"]


def publish(
    path: Union[str, Path],
    *,
    description: str,
    tags: Optional[Union[str, Iterable[str]]] = None,
    **kwargs: Any,
) -> str:
    """``heso registry publish <plat-file> -d "..." [-t "tag1,tag2"]``."""
    if not isinstance(description, str) or not description.strip():
        raise HesoError("registry.publish: `description=` is required (CLI flag -d)")
    argv: list = [str(path), "-d", description]
    if tags is not None:
        csv = tags if isinstance(tags, str) else ",".join(str(t) for t in tags)
        if csv:
            argv.extend(["-t", csv])
    argv.extend(_kwargs_to_argv(kwargs))
    return run("registry", "publish", *argv, parse_json=False)


def pull(hash: str, **kwargs: Any) -> str:
    """``heso registry pull <plat-hash> [-o output-path]``."""
    return run("registry", "pull", str(hash), *_kwargs_to_argv(kwargs), parse_json=False)


def list(**kwargs: Any) -> str:  # noqa: A001 — mirrors the CLI verb
    """``heso registry list [-q query] [-t tag] [--sort ...] [--limit N]``."""
    return run("registry", "list", *_kwargs_to_argv(kwargs), parse_json=False)


def search(query: str, **kwargs: Any) -> dict:
    """``heso registry search <query>`` — multi-source web search."""
    return run("registry", "search", query, *_kwargs_to_argv(kwargs))
