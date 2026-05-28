"""Setuptools shim that installs the prebuilt heso binary onto PATH.

Project metadata lives in pyproject.toml. This file exists for one
reason: to hand the bundled ``heso`` executable to setuptools as a
``scripts`` entry. setuptools copies ``scripts`` verbatim into the
wheel's ``*.data/scripts/`` directory, which pip installs straight
into the environment's bin/Scripts directory. The result is that the
``heso`` command on PATH is the native binary itself -- no Python
interpreter in the hot path.

The binary is staged into ``python/heso/bin/`` before the wheel is
built (locally by the release script, in CI by pypi.yml). Exactly one
of ``heso`` / ``heso.exe`` is present per platform build; whichever
exists is the one shipped to the scripts directory. It is also kept as
package data under ``heso/bin/`` so the importable library
(``import heso``) can resolve it via ``heso._find_binary()``.
"""

import tokenize
from pathlib import Path

from setuptools import setup

_ROOT = Path(__file__).parent
_BIN_DIR = _ROOT / "python" / "heso" / "bin"

# setuptools requires `scripts` entries to be relative, /-separated
# paths from the setup.py directory.
scripts = [
    f"python/heso/bin/{name}"
    for name in ("heso", "heso.exe")
    if (_BIN_DIR / name).is_file()
]

# setuptools' `build_scripts` command opens every `scripts` entry with
# `tokenize.open` to detect and rewrite a `#!python` shebang. Our entry
# is the native `heso` executable, not Python text, so the tokenizer
# rejects it (`source code cannot contain null bytes` on Linux, `invalid
# or missing encoding declaration` on Windows) and the wheel build dies.
#
# Subclassing the command to skip tokenization is the obvious fix, but
# importing any `setuptools._distutils.command` module at setup.py load
# time destabilizes setuptools' build-requirements pass (it runs setup.py
# under a stripped distribution and crashes on the side effects of that
# import). So instead we widen `tokenize.open` itself: on a file the
# tokenizer can't decode, fall back to a byte-tolerant text stream. The
# command then reads a first line that doesn't match the shebang regex
# and copies the file through verbatim -- exactly what we want for a
# binary. Inline scripts keep their normal shebang handling.
_orig_tokenize_open = tokenize.open


def _binary_tolerant_open(filename):
    try:
        return _orig_tokenize_open(filename)
    except (SyntaxError, UnicodeError):
        return open(filename, "r", encoding="latin-1", newline="")


tokenize.open = _binary_tolerant_open

setup(scripts=scripts)
