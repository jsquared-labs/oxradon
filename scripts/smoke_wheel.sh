#!/bin/sh
# Test one built wheel the way users get it. Run from the repository root by release.yml, on a
# machine (or Alpine container) of the wheel's platform, with python3, cargo and uv on PATH.
#
# Usage: scripts/smoke_wheel.sh WHEEL PLATFORM VERSION PEP440 RECORD_OUT
#
# VERSION is Cargo.toml's and PEP440 the same as the wheel spells it (both from release.yml's
# guard job). RECORD_OUT, which ties the wheel's hash to the binary the tests ran on, is written
# only if every check passed.
set -eu

wheel=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
platform=$2
version=$3
pep440=$4
record=$5
root=$(pwd)
work=$(mktemp -d)
links=$(dirname "$wheel")

echo "== pip install"
python3 -m venv "$work/venv"
"$work/venv/bin/pip" install -q --no-index "$wheel"
bin=$work/venv/bin/oxradon
test "$("$bin" --version)" = "oxradon $version"

echo "== run from outside the checkout"
(cd "$work" && "$bin" cc -s "$root/tests/fixtures/branches.py" > /dev/null)

echo "== test suite against the installed binary"
OXRADON_BIN=$bin cargo test --locked --no-fail-fast

echo "== uv tool install and uvx"
export UV_CACHE_DIR="$work/uv-cache" UV_TOOL_DIR="$work/uv-tools" UV_TOOL_BIN_DIR="$work/uv-bin"
uv tool install -q --no-index --find-links "$links" "oxradon==$pep440"
test "$("$work/uv-bin/oxradon" --version)" = "oxradon $version"
# uvx in its own cache and --isolated, so it can't reuse the tool installed just above.
test "$(UV_CACHE_DIR="$work/uvx-cache" uvx -q --isolated --no-index --find-links "$links" \
    --from "oxradon==$pep440" oxradon --version)" = "oxradon $version"

echo "== side by side with radon"
python3 -m pip download -q --dest "$work/wheelhouse" radon==6.0.1
for order in radon-first oxradon-first; do
    python3 -m venv "$work/$order"
    pip=$work/$order/bin/pip
    if [ "$order" = radon-first ]; then
        "$pip" install -q --no-index --find-links "$work/wheelhouse" radon==6.0.1
        "$pip" install -q --no-index "$wheel"
    else
        "$pip" install -q --no-index "$wheel"
        "$pip" install -q --no-index --find-links "$work/wheelhouse" radon==6.0.1
    fi
    "$work/$order/bin/radon" cc -s tests/fixtures/branches.py > "$work/radon.txt"
    "$work/$order/bin/oxradon" cc -s tests/fixtures/branches.py > "$work/oxradon.txt"
    cmp "$work/radon.txt" "$work/oxradon.txt"
done

echo "== record"
python3 scripts/release.py smoke-record "$platform" "$wheel" "$bin" "$record" "$pep440"
