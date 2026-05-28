"""``python -m heso`` entry point.

``pip install heso`` puts the native ``heso`` binary on ``PATH``
directly (shipped via the wheel's ``*.data/scripts/`` directory), so
``heso open URL`` from a shell runs the binary with no Python
interpreter in the way.

This module provides the ``python -m heso open URL`` spelling for
callers who want to drive the binary through the interpreter.
:func:`main` locates the bundled Rust binary in ``heso/bin/`` and
re-execs into it, passing the user's ``sys.argv[1:]`` through
unchanged.
"""

from __future__ import annotations

import os
import sys

from . import HesoError, _find_binary


def main() -> int:
    """Locate the bundled heso binary and exec into it.

    This is the ``python -m heso`` path: it finds the native binary
    bundled in the wheel and re-execs into it, passing ``sys.argv[1:]``
    through unchanged. Returns the binary's exit code so callers waiting
    on ``python -m heso`` exit with the right status. On Unix this never
    returns
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
