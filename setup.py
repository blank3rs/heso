"""Setuptools shim that installs the prebuilt heso binary onto PATH.

Project metadata lives in pyproject.toml. This file exists for one
reason: to hand the bundled ``heso`` executable to setuptools as a
``scripts`` entry. setuptools copies ``scripts`` verbatim into the
wheel's ``*.data/scripts/`` directory, which pip installs straight
into the environment's bin/Scripts directory. The result is that the
``heso`` command on PATH is the native binary itself — no Python
interpreter in the hot path.

The binary is staged into ``python/heso/bin/`` before the wheel is
built (locally by the release script, in CI by pypi.yml). Exactly one
of ``heso`` / ``heso.exe`` is present per platform build; whichever
exists is the one shipped to the scripts directory. It is also kept as
package data under ``heso/bin/`` so the importable library
(``import heso``) can resolve it via ``heso._find_binary()``.
"""

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

setup(scripts=scripts)
