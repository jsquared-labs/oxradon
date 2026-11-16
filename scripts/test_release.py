"""Tests for release.py. Run: python3 -m unittest discover -s scripts -p 'test_release.py'"""

import io
import json
import os
import sys
import tempfile
import unittest
import urllib.error
import zipfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import release  # noqa: E402
from release import PLATFORMS, Refused  # noqa: E402

V = "0.1.0rc1"


def make_wheel(directory, platform, version=V, binary=b"bin", extra=None, meta_version=None):
    name = release.wheel_name(version, PLATFORMS[platform])
    path = Path(directory) / name
    info = f"oxradon-{version}.dist-info"
    with zipfile.ZipFile(path, "w") as z:
        z.writestr(f"oxradon-{version}.data/scripts/oxradon", binary)
        z.writestr(f"{info}/METADATA", f"Metadata-Version: 2.4\nName: oxradon\nVersion: {meta_version or version}\n\n# readme\n")
        z.writestr(f"{info}/WHEEL", "Wheel-Version: 1.0\n")
        z.writestr(f"{info}/RECORD", "")
        z.writestr(f"{info}/licenses/LICENSE", "MIT")
        z.writestr(f"{info}/sboms/oxradon.cyclonedx.json", "{}")
        if extra:
            z.writestr(extra, "x")
    return path


class Route(unittest.TestCase):
    def test_rehearsal_only_by_hand_with_a_prerelease(self):
        self.assertEqual(release.route(release.TESTPYPI_REPO, "workflow_dispatch", "refs/heads/pypi", "0.1.0-rc.1"), "testpypi")
        with self.assertRaises(Refused):
            release.route(release.TESTPYPI_REPO, "push", "refs/tags/v0.1.0-rc.1", "0.1.0-rc.1")
        with self.assertRaises(Refused):
            release.route(release.TESTPYPI_REPO, "workflow_dispatch", "refs/heads/pypi", "0.1.0")

    def test_production_only_on_the_matching_tag(self):
        self.assertEqual(release.route(release.PRODUCTION_REPO, "push", "refs/tags/v0.1.0", "0.1.0"), "production")
        self.assertEqual(release.route(release.PRODUCTION_REPO, "workflow_dispatch", "refs/tags/v0.1.0", "0.1.0"), "production")
        for ref in ("refs/tags/v0.1.1", "refs/heads/main", "refs/tags/0.1.0"):
            with self.assertRaises(Refused):
                release.route(release.PRODUCTION_REPO, "push", ref, "0.1.0")
        with self.assertRaises(Refused):
            release.route(release.PRODUCTION_REPO, "pull_request", "refs/tags/v0.1.0", "0.1.0")

    def test_other_repositories_refused(self):
        for repo in ("someone/oxradon", "jsquared-labs/oxradon-fork", "oxradon"):
            with self.assertRaises(Refused):
                release.route(repo, "workflow_dispatch", "refs/tags/v0.1.0", "0.1.0")


class Pep440(unittest.TestCase):
    def test_conversion(self):
        self.assertEqual(release.pep440("0.1.0"), "0.1.0")
        self.assertEqual(release.pep440("0.1.0-rc.1"), "0.1.0rc1")
        self.assertEqual(release.pep440("1.2.3-alpha.4"), "1.2.3a4")
        self.assertEqual(release.pep440("1.2.3-beta.10"), "1.2.3b10")
        for bad in ("0.1", "0.1.0-dev.1", "0.1.0-rc1", "0.1.0+build"):
            with self.assertRaises(Refused):
                release.pep440(bad)


class Wheels(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_check_wheel(self):
        self.assertEqual(release.check_wheel(make_wheel(self.dir, "macos-aarch64"), V), "macos-aarch64")
        with self.assertRaises(Refused):
            release.check_wheel(make_wheel(self.dir, "macos-x86_64", extra="oxradon/__init__.py"), V)
        with self.assertRaises(Refused):
            release.check_wheel(make_wheel(self.dir, "linux-gnu-x86_64", meta_version="0.1.0"), V)
        with self.assertRaises(Refused):
            release.check_wheel(make_wheel(self.dir, "linux-gnu-aarch64", version="0.1.0"), V)

    def full_set(self):
        dist, records = self.dir / "dist", self.dir / "records"
        dist.mkdir()
        records.mkdir()
        installed = self.dir / "installed"
        installed.write_bytes(b"bin")
        for platform in PLATFORMS:
            wheel = make_wheel(dist, platform)
            release.smoke_record(platform, wheel, installed, records / f"{platform}.json", V)
        return dist, records

    def test_assemble_accepts_six_tested_wheels(self):
        dist, records = self.full_set()
        manifest = release.assemble(dist, records, V, self.dir / "manifest.json")
        self.assertEqual(len(manifest["files"]), 6)
        release.verify_dist(dist, self.dir / "manifest.json")

    def test_assemble_refuses_gaps_and_substitutes(self):
        dist, records = self.full_set()
        (records / "macos-x86_64.json").unlink()
        with self.assertRaises(Refused):
            release.assemble(dist, records, V, self.dir / "m.json")
        dist, records = self.dir / "dist", self.dir / "records"
        installed = self.dir / "installed"
        release.smoke_record("macos-x86_64", dist / release.wheel_name(V, PLATFORMS["macos-x86_64"]), installed, records / "macos-x86_64.json", V)
        make_wheel(dist, "linux-musl-x86_64", binary=b"other")  # rebuilt after testing
        with self.assertRaises(Refused):
            release.assemble(dist, records, V, self.dir / "m.json")

    def test_assemble_refuses_sdist(self):
        dist, records = self.full_set()
        (dist / f"oxradon-{V}.tar.gz").write_bytes(b"")
        with self.assertRaises(Refused):
            release.assemble(dist, records, V, self.dir / "m.json")

    def test_smoke_record_refuses_a_different_installed_binary(self):
        wheel = make_wheel(self.dir, "macos-aarch64")
        installed = self.dir / "installed"
        installed.write_bytes(b"not the same")
        with self.assertRaises(Refused):
            release.smoke_record("macos-aarch64", wheel, installed, self.dir / "r.json", V)

    def test_verify_dist_refuses_a_changed_file(self):
        dist, records = self.full_set()
        release.assemble(dist, records, V, self.dir / "manifest.json")
        make_wheel(dist, "macos-aarch64", binary=b"changed")
        with self.assertRaises(Refused):
            release.verify_dist(dist, self.dir / "manifest.json")


class Crate(unittest.TestCase):
    def test_crate_list(self):
        tracked = {"src/lib.rs", "Cargo.lock", "README.md", "scripts/bench.py"}
        release.crate_list(["src/lib.rs", "Cargo.lock", "Cargo.toml", "Cargo.toml.orig", ".cargo_vcs_info.json"], tracked)
        with self.assertRaises(Refused):
            release.crate_list(["src/lib.rs", "notes.txt"], tracked)
        with self.assertRaises(Refused):
            release.crate_list(["scripts/bench.py"], tracked)

    def test_crates_io(self):
        with tempfile.TemporaryDirectory() as tmp:
            crate = Path(tmp) / "oxradon-0.1.0.crate"
            crate.write_bytes(b"crate")
            digest = release.sha256(crate)
            self.assertEqual(release.crates_io("0.1.0", crate, fetch=lambda url: None), "absent")
            same = {"version": {"checksum": digest}}
            self.assertEqual(release.crates_io("0.1.0", crate, fetch=lambda url: same), "published")
            with self.assertRaises(Refused):
                release.crates_io("0.1.0", crate, fetch=lambda url: {"version": {"checksum": "0" * 64}})


class Fetch(unittest.TestCase):
    def opener(self, *outcomes):
        """A urlopen that gives each outcome in turn: an int is an HTTP error code, an exception
        is raised, anything else is returned as the JSON body."""
        calls = []

        def open_url(request, timeout):
            outcome = outcomes[len(calls)]
            calls.append(request.full_url)
            if isinstance(outcome, int):
                raise urllib.error.HTTPError(request.full_url, outcome, "x", {}, io.BytesIO())
            if isinstance(outcome, Exception):
                raise outcome
            return io.BytesIO(json.dumps(outcome).encode())

        return open_url, calls

    def test_retries_transient_errors_only(self):
        sleeps = []
        open_url, calls = self.opener(503, TimeoutError(), 429, {"ok": 1})
        self.assertEqual(release._fetch_json("https://registry.test/x", open_url, sleeps.append), {"ok": 1})
        self.assertEqual(len(calls), 4)
        self.assertEqual(len(sleeps), 3)
        open_url, _ = self.opener(404)
        self.assertIsNone(release._fetch_json("https://registry.test/x", open_url, sleeps.append))
        open_url, calls = self.opener(403)
        with self.assertRaises(urllib.error.HTTPError):
            release._fetch_json("https://registry.test/x", open_url, sleeps.append)
        self.assertEqual(len(calls), 1)
        open_url, calls = self.opener(500, 500, 500)
        with self.assertRaises(urllib.error.HTTPError):
            release._fetch_json("https://registry.test/x", open_url, sleeps.append, attempts=3)
        self.assertEqual(len(calls), 3)


if __name__ == "__main__":
    unittest.main()
