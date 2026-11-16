"""Differential fuzzing of `raw`: generated files through `radon raw -j` and `oxradon raw -j`.

Usage: python -I scripts/fuzz_raw.py RADON OXRADON N [SEED]

Writes N generated sources (scripts/_rawgen.py) into scratch directories in batches, runs both
tools on each batch, and compares every file's JSON entry exactly, error messages included. Exits
non-zero on any difference and keeps the differing files for inspection.
"""

import json
import os
import random
import shutil
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _rawgen  # noqa: E402
from _common import run, usage  # noqa: E402

BATCH = 500


def main():
    if len(sys.argv) < 4:
        usage(__doc__)
    radon, rs, n, *rest = sys.argv[1:]
    n = int(n)
    seed = int(rest[0]) if rest else 0
    rng = random.Random(seed)
    keep = tempfile.mkdtemp(prefix="fuzz_raw_diffs_")
    differ = 0
    done = 0
    while done < n:
        count = min(BATCH, n - done)
        with tempfile.TemporaryDirectory(prefix="fuzz_raw_") as tmp:
            for i in range(count):
                text = _rawgen.source(rng)
                data = text.encode("utf-8", "surrogatepass")
                if rng.random() < 0.01:
                    data += b"\xff"  # an undecodable byte now and then
                with open(os.path.join(tmp, f"c{done + i:06d}.py"), "wb") as f:
                    f.write(data)
            a_raw = run([radon, "raw", "-j", tmp], text=False)
            b_raw = run([rs, "raw", "-j", tmp], text=False)
            if a_raw != b_raw:
                a, b = json.loads(a_raw), json.loads(b_raw)
                for name in sorted(set(a) | set(b)):
                    if a.get(name) != b.get(name):
                        differ += 1
                        if differ <= 20:
                            print(f"{os.path.basename(name)}: radon={a.get(name)} rs={b.get(name)}")
                        shutil.copy(name, keep)
                if a == b:
                    differ += 1
                    print(f"batch at {done}: same JSON values, different bytes")
        done += count
    print(f"{n} cases (seed {seed}): {differ} differ")
    if differ:
        print(f"differing inputs kept in {keep}")
    else:
        shutil.rmtree(keep)
    sys.exit(1 if differ else 0)


if __name__ == "__main__":
    main()
