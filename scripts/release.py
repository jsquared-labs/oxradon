"""Checks used by .github/workflows/release.yml. Each one exits non-zero on any doubt.

Usage: python3 scripts/release.py COMMAND [ARGS]

  route                      decide the destination from the GitHub environment (see ROUTES)
  check-wheel WHEEL VERSION  the wheel holds only the binary, metadata and license, at VERSION
  smoke-record PLATFORM WHEEL INSTALLED OUT VERSION
                             after the tests passed on INSTALLED: write a record tying the
                             wheel's hash to the binary that was tested
  assemble DIST RECORDS VERSION OUT
                             exactly one tested wheel per platform and nothing else; write the
                             manifest of names and hashes that publishing must match
  verify-dist DIST MANIFEST  the files about to be published are the manifest's, byte for byte
  crate-list LIST            every file `cargo package --list` wrote to LIST is tracked in git
                             and none is excluded (Cargo's own generated files allowed)
  crates-io VERSION CRATE    whether VERSION is on crates.io: `absent`, or `published` when its
                             checksum equals CRATE's SHA-256 (refused when it differs)

The rules are in the spec (Spec P, revision 3): one version source (Cargo.toml), six wheels,
no sdist, and the published files are the tested files.
"""

import hashlib
import json
import os
import re
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.request
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Release destinations by repository and trigger. Anything else is refused.
TESTPYPI_REPO = "jsquared-labs/oxradon-ci"
PRODUCTION_REPO = "jsquared-labs/oxradon"

# The six wheels, by the platform tag maturin gives them (as maturin 1.16 orders them).
PLATFORMS = {
    "linux-gnu-x86_64": "manylinux2014_x86_64.manylinux_2_17_x86_64",
    "linux-gnu-aarch64": "manylinux2014_aarch64.manylinux_2_17_aarch64",
    "linux-musl-x86_64": "musllinux_1_2_x86_64",
    "linux-musl-aarch64": "musllinux_1_2_aarch64",
    "macos-aarch64": "macosx_11_0_arm64",
    "macos-x86_64": "macosx_10_12_x86_64",
}

SEMVER = re.compile(r"(\d+)\.(\d+)\.(\d+)(?:-(alpha|beta|rc)\.(\d+))?")
PEP440_PRE = {"alpha": "a", "beta": "b", "rc": "rc"}


class Refused(Exception):
    """A check failed; the message says which and why."""


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def cargo_version(root=ROOT):
    with open(root / "Cargo.toml", "rb") as f:
        return tomllib.load(f)["package"]["version"]


def pep440(version):
    """Cargo's `X.Y.Z[-alpha|beta|rc.N]` as PEP 440 (what maturin writes); other forms refused."""
    m = SEMVER.fullmatch(version)
    if not m:
        raise Refused(f"version {version!r} is not X.Y.Z or X.Y.Z-(alpha|beta|rc).N")
    major, minor, patch, pre, n = m.groups()
    return f"{major}.{minor}.{patch}" + (f"{PEP440_PRE[pre]}{n}" if pre else "")


def route(repository, event, ref, version):
    """(destination, reason) for this run, or Refused."""
    pre = "-" in version
    if repository == TESTPYPI_REPO:
        if event != "workflow_dispatch":
            raise Refused(f"{repository} publishes only when started by hand, not on {event}")
        if not pre:
            raise Refused(f"a TestPyPI rehearsal needs a pre-release version, not {version}")
        return "testpypi"
    if repository == PRODUCTION_REPO:
        if event not in ("push", "workflow_dispatch"):
            raise Refused(f"unexpected trigger {event}")
        if ref != f"refs/tags/v{version}":
            raise Refused(f"production runs only on the tag v{version} (Cargo.toml), not {ref}")
        return "production"
    raise Refused(f"repository {repository} may not publish")


def wheel_name(version, platform_tag):
    return f"oxradon-{version}-py3-none-{platform_tag}.whl"


def check_wheel(path, version):
    """The wheel's name, metadata version and contents."""
    path = Path(path)
    tags = {wheel_name(version, tag): key for key, tag in PLATFORMS.items()}
    if path.name not in tags:
        raise Refused(f"{path.name} is not one of the expected wheel names for {version}")
    data = f"oxradon-{version}.data"
    info = f"oxradon-{version}.dist-info"
    allowed = {f"{data}/scripts/oxradon", f"{info}/METADATA", f"{info}/WHEEL", f"{info}/RECORD",
               f"{info}/licenses/LICENSE"}
    with zipfile.ZipFile(path) as z:
        names = set(z.namelist())
        extra = {n for n in names - allowed if not re.fullmatch(rf"{re.escape(info)}/sboms/[\w.-]+\.json", n)}
        if extra:
            raise Refused(f"{path.name} has unexpected files: {sorted(extra)}")
        missing = allowed - names
        if missing:
            raise Refused(f"{path.name} lacks {sorted(missing)}")
        meta = z.read(f"{info}/METADATA").decode()
    fields = dict(line.split(": ", 1) for line in meta.split("\n\n", 1)[0].splitlines() if ": " in line)
    if fields.get("Name") != "oxradon" or fields.get("Version") != version:
        raise Refused(f"{path.name}: metadata says {fields.get('Name')} {fields.get('Version')}")
    return tags[path.name]


def binary_hash(wheel, version):
    with zipfile.ZipFile(wheel) as z:
        return hashlib.sha256(z.read(f"oxradon-{version}.data/scripts/oxradon")).hexdigest()


def smoke_record(platform, wheel, installed, out, version):
    if platform not in PLATFORMS:
        raise Refused(f"unknown platform {platform}")
    if check_wheel(wheel, version) != platform:
        raise Refused(f"{Path(wheel).name} is not the {platform} wheel")
    inside, here = binary_hash(wheel, version), sha256(installed)
    if inside != here:
        raise Refused(f"installed binary {here} differs from the wheel's {inside}")
    record = {"platform": platform, "wheel": Path(wheel).name, "wheel_sha256": sha256(wheel),
              "binary_sha256": here}
    Path(out).write_text(json.dumps(record, indent=1) + "\n")
    return record


def assemble(dist, records, version, out):
    dist, records = Path(dist), Path(records)
    files = sorted(p for p in dist.iterdir() if p.is_file())
    wheels = [p for p in files if p.suffix == ".whl"]
    others = [p.name for p in files if p.suffix != ".whl"]
    if others:
        raise Refused(f"unexpected files in the distribution: {others}")
    seen = {}
    for wheel in wheels:
        platform = check_wheel(wheel, version)
        if platform in seen:
            raise Refused(f"two wheels for {platform}")
        seen[platform] = wheel
    missing = sorted(set(PLATFORMS) - set(seen))
    if missing:
        raise Refused(f"no wheel for {missing}")
    manifest = {"version": version, "files": {}}
    for platform, wheel in sorted(seen.items()):
        record_path = records / f"{platform}.json"
        if not record_path.is_file():
            raise Refused(f"no passing smoke record for {platform}")
        record = json.loads(record_path.read_text())
        digest = sha256(wheel)
        if record["wheel"] != wheel.name or record["wheel_sha256"] != digest:
            raise Refused(f"{wheel.name} is not the file the {platform} tests ran on")
        if record["binary_sha256"] != binary_hash(wheel, version):
            raise Refused(f"{wheel.name}: tested binary differs from the one inside")
        manifest["files"][wheel.name] = digest
    Path(out).write_text(json.dumps(manifest, indent=1) + "\n")
    return manifest


def verify_dist(dist, manifest_path):
    manifest = json.loads(Path(manifest_path).read_text())["files"]
    here = {p.name: sha256(p) for p in Path(dist).iterdir() if p.is_file()}
    if here != manifest:
        raise Refused(f"distribution differs from the tested manifest:\n here {here}\n tested {manifest}")


# Cargo writes these into every package; they are not in git.
CARGO_GENERATED = {"Cargo.toml", "Cargo.toml.orig", ".cargo_vcs_info.json"}
# Kept out of the crate by Cargo.toml's `exclude`; a regression there is refused here.
NOT_IN_CRATE = ("scripts/", "site/", ".github/", "REVIEW-NOTE.md", "pyproject.toml", "clippy.toml")


def crate_list(listed, tracked):
    """`listed`: the package's files; `tracked`: git's. Refuse untracked or excluded files."""
    bad = sorted(f for f in listed if f.startswith(NOT_IN_CRATE))
    if bad:
        raise Refused(f"excluded files in the crate: {bad}")
    untracked = sorted(f for f in listed if f not in tracked and f not in CARGO_GENERATED)
    if untracked:
        raise Refused(f"files in the crate that git does not track: {untracked}")


def crates_io(version, crate, fetch=None):
    fetch = fetch or _fetch_json
    data = fetch(f"https://crates.io/api/v1/crates/oxradon/{version}")
    if data is None:
        return "absent"
    remote = data["version"]["checksum"]
    if remote != sha256(crate):
        raise Refused(f"oxradon {version} on crates.io ({remote}) differs from the tested crate")
    return "published"


def _fetch_json(url, open_url=urllib.request.urlopen, sleep=time.sleep, attempts=5):
    """The JSON at `url`, or None on 404. Timeouts, connection errors, 429 and 5xx are retried
    with backoff, so a registry hiccup doesn't fail a release that has already published."""
    # crates.io asks automated clients to say who they are.
    request = urllib.request.Request(url, headers={
        "User-Agent": "oxradon-release (rob@jsquaredlabs.com)",
        "Accept": "application/json",
    })
    for attempt in range(attempts):
        try:
            with open_url(request, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            if e.code != 429 and e.code < 500 or attempt == attempts - 1:
                raise
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            if attempt == attempts - 1:
                raise
        sleep(min(2 ** attempt * 2, 30))


def _output(**values):
    lines = "".join(f"{k}={v}\n" for k, v in values.items())
    print(lines, end="")
    if "GITHUB_OUTPUT" in os.environ:
        with open(os.environ["GITHUB_OUTPUT"], "a") as f:
            f.write(lines)


def main(argv):
    if not argv:
        raise Refused(__doc__)
    command, args = argv[0], argv[1:]
    if command == "route" and not args:
        version = cargo_version()
        env = os.environ
        destination = route(env["GITHUB_REPOSITORY"], env["GITHUB_EVENT_NAME"], env["GITHUB_REF"], version)
        _output(destination=destination, version=version, pep440=pep440(version))
    elif command == "check-wheel" and len(args) == 2:
        print(check_wheel(args[0], args[1]))
    elif command == "smoke-record" and len(args) == 5:
        print(json.dumps(smoke_record(*args)))
    elif command == "assemble" and len(args) == 4:
        manifest = assemble(*args)
        for name, digest in manifest["files"].items():
            print(f"| `{name}` | `{digest}` |")
    elif command == "verify-dist" and len(args) == 2:
        verify_dist(*args)
    elif command == "crate-list" and len(args) == 1:
        listed = Path(args[0]).read_text().split()
        tracked = set(subprocess.run(["git", "ls-files"], cwd=ROOT, check=True, capture_output=True,
                                     text=True).stdout.split("\n"))
        crate_list(listed, tracked)
    elif command == "crates-io" and len(args) == 2:
        _output(state=crates_io(*args))
    else:
        raise Refused(__doc__)


if __name__ == "__main__":
    try:
        main(sys.argv[1:])
    except Refused as e:
        print(f"refused: {e}", file=sys.stderr)
        sys.exit(1)
