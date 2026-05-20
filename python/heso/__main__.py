"""CLI entry point for the ``heso`` console script.

``pip install heso`` installs a Windows ``heso.exe`` (or POSIX
``heso``) generated from this module's :func:`main` via the
``[project.scripts]`` entry in :file:`pyproject.toml`. When invoked,
:func:`main` locates the bundled Rust binary in ``heso/bin/`` and
re-execs into it, passing the user's ``sys.argv[1:]`` through
unchanged.

This indirection is what lets the same wheel ship both a library
(``import heso; heso.open(...)``) and a working CLI (``heso open
URL``) — the binary is the source of truth either way; this is just
how Python users get it on ``PATH``.

``python -m heso open URL`` also works via the ``__main__`` module
hook.
"""

from __future__ import annotations

import os
import sys

from . import HesoError, _find_binary


def main() -> int:
    """Locate the bundled heso binary and exec into it.

    Returns the binary's exit code so ``[project.scripts]``-generated
    wrappers exit with the right status. On Unix this never returns
    (``os.execv`` replaces the current process); on Windows we fall
    back to :func:`subprocess.call` because ``execv`` semantics on
    Windows don't replace the process the way POSIX does — Python's
    docs warn that the parent's exit happens before the child does,
    which breaks parent-waits-on-child callers (pip's wrapper, CI
    runners).
    """
    try:
        binary = _find_binary()
    except HesoError as e:
        sys.stderr.write(f"{e}\n")
        return 1

    argv = [binary, *sys.argv[1:]]

    if os.name == "nt":
        # On Windows `os.execv` returns control to the parent before
        # the child finishes, which confuses every parent process
        # waiting on the script's exit code. Spawn-and-wait instead.
        # subprocess.call propagates Ctrl+C cleanly.
        import subprocess

        return subprocess.call(argv)

    # POSIX: replace this process with the binary. No return.
    os.execv(binary, argv)
    return 0  # unreachable


if __name__ == "__main__":
    sys.exit(main())
