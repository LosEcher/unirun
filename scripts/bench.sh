#!/usr/bin/env bash
# unirun end-to-end performance benchmark (release binary).
#
# Uses hyperfine when available (recommended), else a bash timing loop.
# Results are indicative of real agent use: full process spawn + argv
# handoff + output capture + encoding + classification.
#
#   scripts/bench.sh [path-to-unirun-binary]

set -euo pipefail

BIN="${1:-target/release/unirun}"
if [ ! -x "$BIN" ]; then
  echo "building release binary…"
  cargo build --release
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
echo "benchmarking: $BIN"

if command -v hyperfine >/dev/null 2>&1; then
  hyperfine --warmup 2 \
    --export-json /tmp/unirun-bench.json \
    -n "run 'true'"            "$BIN run 'true'" \
    -n "run echo hello"        "$BIN run 'echo hello'" \
    -n "run --json echo"       "$BIN run 'echo hello' --json" \
    -n "unicode output"        "$BIN run 'echo 中文输出'" \
    -n "large output 1 MiB"    "$BIN run 'yes x | head -c 1048576' >/dev/null" \
    -n "timeout kill (1s)"     "$BIN run 'sleep 30' --timeout 1 || true"
  echo "machine-readable: /tmp/unirun-bench.json"
  exit 0
fi

echo "hyperfine not found — using a python3 timing loop (median of 7)"
python3 - "$BIN" <<'PY'
import subprocess, sys, time, statistics
bin = sys.argv[1]
cases = [
    ("run 'true'",            [bin, "run", "true"]),
    ("run echo hello",        [bin, "run", "echo hello"]),
    ("run --json echo",       [bin, "run", "echo hello", "--json"]),
    ("unicode output",        [bin, "run", "echo 中文输出"]),
    ("large output 1 MiB",    [bin, "run", "yes x | head -c 1048576"]),
]
for name, argv in cases:
    samples = []
    for _ in range(7):
        t0 = time.perf_counter()
        subprocess.run(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        samples.append((time.perf_counter() - t0) * 1000)
    samples.sort()
    print(f"{name:<24} median {statistics.median(samples):7.2f} ms   min {samples[0]:7.2f} ms")
PY
