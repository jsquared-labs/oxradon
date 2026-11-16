"""Benchmark radon against oxradon (best of N wall-clock runs).

Usage: python scripts/bench.py RADON OXRADON [--command cc|hal|raw|mi] PATH [PATH ...]

Each command runs with the flags below. `hal` is timed with `-j`: in terminal mode radon stops at
the first file it cannot parse (a COMPATIBILITY.md row), so it would not analyze the whole tree.
"""

import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import RADON_ENV, run, usage


def check_completes(cmd):
    """Run `cmd` once, untimed. An abort still exits 0 (radon's `main`
    catches it and prints `ERROR:` at column 0), so a run that stopped early
    would otherwise be timed as if it had analyzed everything."""
    out = run(cmd)
    aborts = [line for line in out.splitlines() if line.startswith("ERROR:")]
    if aborts:
        sys.exit(f"{cmd[0]} aborted the run: {aborts[0][:300]}")


def best(cmd, runs, env=None):
    """Best wall-clock time of `runs` runs; any failed run aborts the benchmark."""
    env = dict(RADON_ENV, **(env or {}))
    times = []
    for _ in range(runs):
        start = time.perf_counter()
        proc = subprocess.run(
            cmd, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env
        )
        times.append(time.perf_counter() - start)
        if proc.returncode != 0:
            sys.exit(f"{cmd[0]} exited {proc.returncode}: {proc.stderr.decode(errors='replace')[-500:]}")
    return min(times)


# The flags each command is timed with, and how many times radon runs (radon's `raw` and `mi`
# take tens of seconds on a large tree; oxradon always runs the full count).
COMMANDS = {"cc": (["-s"], 7), "hal": (["-j"], 3), "raw": (["-s"], 3), "mi": (["-s"], 3)}


def main():
    args = sys.argv[1:]
    command = "cc"
    if "--command" in args:
        i = args.index("--command")
        command = args[i + 1]
        del args[i:i + 2]
    if command not in COMMANDS or len(args) < 3:
        usage(__doc__)
    flags, radon_runs = COMMANDS[command]
    radon, rs, *paths = args
    small = next(
        (os.path.join(paths[0], f) for f in sorted(os.listdir(paths[0])) if f.endswith(".py")),
        None,
    )
    cases = [(f"{', '.join(paths)}", paths, 7, radon_runs)]
    if small:
        cases.append((f"one file ({os.path.basename(small)})", [small], 10, 10))
    for label, args, runs, slow_runs in cases:
        cmd = [command, *flags, *args]
        for tool in (radon, rs):
            check_completes([tool, *cmd])
        a = best([radon, *cmd], slow_runs)
        b = best([rs, *cmd], runs)
        c = best([rs, *cmd], runs, {"RAYON_NUM_THREADS": "1"})
        print(f"{command} {' '.join(flags)}: {label}")
        print(f"  radon            {a * 1000:8.1f} ms")
        print(f"  oxradon         {b * 1000:8.1f} ms  ({a / b:.0f}x)")
        print(f"  oxradon 1 core  {c * 1000:8.1f} ms  ({a / c:.0f}x)")


if __name__ == "__main__":
    main()
