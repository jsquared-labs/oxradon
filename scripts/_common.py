"""Helpers shared by the scripts in this directory."""

import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

# A minimal, colorless environment, so radon and oxradon see the same settings.
RADON_ENV = {"COLOR": "no", "PATH": "/usr/bin:/bin"}

# radon's per-file error line. Messages differ between CPython and Ruff, so
# comparisons keep only the prefix (tests/golden.rs uses the same marker).
ERROR_PREFIX = "    ERROR: "

# Explicit file lists are passed in chunks, to stay under ARG_MAX.
CHUNK = 500


def usage(doc):
    """Print the usage line from a script's docstring, to stderr, and exit with status 2."""
    lines = (doc or "").splitlines()
    start = next((i for i, line in enumerate(lines) if line.startswith("Usage")), None)
    if start is None:
        sys.exit(2)
    shown = lines[start:start + 1]
    if shown[0].endswith(":") and start + 1 < len(lines):
        shown.append(lines[start + 1])  # "Usage, from the repository root, ...:" + the command
    print("\n".join(line.rstrip() for line in shown), file=sys.stderr)
    sys.exit(2)


def run_status(cmd, env=RADON_ENV, stdin=None, cwd=None):
    """Run a command and return (exit status, stdout decoded with line endings intact).
    A non-zero exit does not abort: the caller compares the status too."""
    proc = subprocess.run(cmd, check=False, capture_output=True, env=env, input=stdin, cwd=cwd)
    return proc.returncode, proc.stdout.decode("utf-8", errors="surrogateescape")


def run(cmd, env=RADON_ENV, stdin=None, *, cwd=None, text=True):
    """Run a command and return stdout: decoded with line endings intact, or as bytes if
    `text` is false. A non-zero exit aborts the script, so a failed run is never compared."""
    proc = subprocess.run(cmd, check=False, capture_output=True, env=env, input=stdin, cwd=cwd)
    if proc.returncode != 0:
        stderr = proc.stderr.decode(errors="replace")[-500:]
        sys.exit(f"{cmd[0]} exited {proc.returncode}: {stderr}")
    if not text:
        return proc.stdout
    return proc.stdout.decode("utf-8", errors="surrogateescape")


def walk_order(path):
    """Every directory under `path` with its entries, in the order the system lists them."""
    return [[root, dirs, files] for root, dirs, files in os.walk(path)]


def run_radon(cmd):
    """`run_status`, but with RADON_CACHE=DIR the output is kept in DIR and reused (delete the
    directory after changing radon or the corpus). Only radon's runs go through here; oxradon's
    are never cached.

    The key is the command and, for each directory it names, that directory's listing order:
    radon reports a walked directory's files in the order the system lists them, which differs
    between machines and between two copies of the same tree, so an output recorded for one
    order must not be reused for another."""
    cache = os.environ.get("RADON_CACHE")
    if not cache:
        return run_status(cmd)
    walked = [walk_order(arg) for arg in cmd[1:] if os.path.isdir(arg)]
    key = hashlib.sha256(json.dumps([cmd, walked]).encode()).hexdigest()
    entry = os.path.join(cache, key + ".json")
    if os.path.exists(entry):
        with open(entry) as f:
            return tuple(json.load(f))
    status, out = run_status(cmd)
    os.makedirs(cache, exist_ok=True)
    with open(entry, "w") as f:
        json.dump([status, out], f)
    return status, out


def chunks(names):
    """`names` in lists of CHUNK."""
    return [names[i:i + CHUNK] for i in range(0, len(names), CHUNK)]


def is_error(value):
    """Whether a `cc -j` entry is radon's per-file error object."""
    return isinstance(value, dict) and "error" in value


def record_config(radon, cases, default_args=None):
    """Run radon in a scratch project for each config case and return the recordings.

    A case maps file names (nested ones get their directories) to text, plus `__args__` (the
    command line, split on spaces; `default_args` if absent, and required when that is None)
    and `__read__` (a file whose text is recorded too). radon runs in the project with HOME
    there and must exit 0."""
    recorded = {}
    for name, files in cases.items():
        files = dict(files)
        args = (files.pop("__args__") if default_args is None
                else files.pop("__args__", default_args)).split()
        read = files.pop("__read__", None)
        with tempfile.TemporaryDirectory() as tmp:
            for rel, text in files.items():
                (Path(tmp) / rel).parent.mkdir(parents=True, exist_ok=True)
                (Path(tmp) / rel).write_text(text)
            env = {**RADON_ENV, "HOME": tmp}
            proc = subprocess.run([radon, *args], check=True, capture_output=True, env=env, cwd=tmp)
            out = {"stdout": proc.stdout.decode()}
            if read:
                out["file"] = (Path(tmp) / read).read_text()
        recorded[name] = {"files": files, "args": args, "read": read, "expected": out}
    return recorded
