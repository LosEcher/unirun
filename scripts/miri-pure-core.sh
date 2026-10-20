#!/usr/bin/env bash
#
# Miri gate for unirun's *syscall-free pure core*.
#
# Why only a subset: Miri cannot execute FFI or real syscalls, so `exec`,
# `process_identity`, `transport`, `session` are out of scope by construction —
# they spawn processes, read /proc, or call libc. The modules targeted here are
# pure data transforms (`encoding`, `coalesce`, `taxonomy`): bytes in, decisions
# out, no clock, no filesystem, no environment. That is the whole point of the
# split — a small core that a machine can prove, and a thin shell around it that
# only integration tests can judge.
#
# This gate is deliberately *not* a "run everything" wrapper. It runs a filter
# list, and it fails when the filter list stops matching enough tests (see
# MIN_PASSED), because a gate that silently matches zero tests is indistinguishable
# from a passing one. The other half of that discipline is `--self-test`, which
# injects real undefined behaviour into throwaway crates and requires Miri to
# report it — a green Miri run proves nothing if the harness cannot go red.
#
# Exit codes (three states, never collapsed):
#   0  pass             — Miri ran the pure core, no UB, test floor met
#   1  fail             — UB found, a test failed, or the floor was not met
#   2  inconclusive     — could not judge (no Miri, no sysroot, unknown failure)
#
# Exit 2 is *not* a pass. A caller that treats it as one has rebuilt the
# "environment failure disguised as a product defect" bug this repo already
# paid for once (2026-10-08: `cargo: command not found` recorded as "real drift").
#
# Usage:
#   scripts/miri-pure-core.sh              # the gate
#   scripts/miri-pure-core.sh --self-test  # negative/positive controls for the judge
#   scripts/miri-pure-core.sh --print      # show the resolved toolchain/sysroot/filters
#
# Environment (all optional; every default is resolved by contract, not by hoping
# the caller's shell is configured):
#   CARGO              cargo to use (default: PATH, then ~/.cargo/bin, then Homebrew)
#   MIRI_TOOLCHAIN     rustup toolchain for Miri (default: nightly)
#   MIRI_SYSROOT       prebuilt Miri sysroot (default: the cargo-miri cache if present)
#   MIRIFLAGS          Miri flags (default: -Zmiri-tree-borrows)
#   MIRI_MIN_PASSED    test floor (default: 34 — raise it when you add tests)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLCHAIN="${MIRI_TOOLCHAIN:-nightly}"
FILTERS=(encoding:: coalesce:: taxonomy::)
MIN_PASSED="${MIRI_MIN_PASSED:-34}"
MIRI_FLAGS="${MIRIFLAGS:--Zmiri-tree-borrows}"

say() { printf '%s\n' "$*"; }

# --- capability probing ----------------------------------------------------
# Never `command -v cargo` alone: a launchd/CI shell can have a PATH without
# ~/.cargo/bin, and the resulting "not found" looks exactly like a broken repo.
resolve_cargo() {
    local candidate
    for candidate in "${CARGO:-}" "$(command -v cargo 2>/dev/null || true)" \
        "$HOME/.cargo/bin/cargo" /opt/homebrew/bin/cargo /usr/local/bin/cargo; do
        if [ -n "$candidate" ] && [ -x "$candidate" ]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

resolve_sysroot() {
    local dir
    if [ -n "${MIRI_SYSROOT:-}" ]; then
        printf '%s' "$MIRI_SYSROOT"
        return 0
    fi
    for dir in "$HOME/Library/Caches/org.rust-lang.miri" "$HOME/.cache/miri"; do
        if [ -d "$dir/lib" ]; then
            printf '%s' "$dir"
            return 0
        fi
    done
    return 1
}

# `#[cfg(miri)]` / `cfg!(miri)` branches are how a gate turns into "green but
# untested": code that only exists under Miri never runs in production, and code
# that vanishes under Miri hides the very paths the gate claims to prove.
# Zero tolerance — none exist today, so any appearance is a deliberate act.
scan_ub_holes() {
    local hits
    hits="$(cd "$REPO_ROOT" && grep -rn -E 'cfg\(miri\)|cfg!\(miri\)|cfg_attr\(miri' src tests 2>/dev/null || true)"
    if [ -n "$hits" ]; then
        printf '%s\n' "$hits"
        return 1
    fi
    return 0
}

# --- judgement (pure: log + exit code in, verdict out) ---------------------
# Kept separate from execution so `--self-test` can feed it synthetic logs. The
# ordering matters: UB text wins, then an explicit libtest failure, then a
# non-zero exit (a non-zero exit must never be readable as a pass just because
# the log happens to contain a happy "test result" line), and only then the
# counts — including the "no result line at all" case, which is exit 2, not 0.
# judge <log> <rc> [floor] → "<code> <verdict> <reason>"
# The floor defaults to the repo's MIN_PASSED; throwaway control crates pass their
# own (they contain one test, not the repo's 34). Getting that wrong once made the
# "clean crate must come back green" control read as a failure — an instrument bug
# that would have been mistaken for a product bug.
judge() {
    local log="$1" rc="$2" floor="${3:-$MIN_PASSED}" passed failed
    if grep -q 'Undefined Behavior' "$log" 2>/dev/null; then
        printf '1 ub undefined-behavior'
        return 0
    fi
    if grep -q 'test result: FAILED' "$log" 2>/dev/null; then
        printf '1 fail test-failed'
        return 0
    fi
    if [ "$rc" -ne 0 ]; then
        printf '2 inconclusive exit-rc%s' "$rc"
        return 0
    fi
    passed="$(sed -n 's/^test result: .* \([0-9][0-9]*\) passed.*/\1/p' "$log" | tail -1)"
    failed="$(sed -n 's/^test result: .* \([0-9][0-9]*\) failed.*/\1/p' "$log" | tail -1)"
    if [ -z "$passed" ]; then
        # No libtest summary line = nothing was measured. This is the "empty
        # gate = false confidence" case; it must not be reachable as a pass.
        printf '2 inconclusive no-test-result-line'
        return 0
    fi
    if [ "${failed:-0}" -ne 0 ]; then
        printf '1 fail failed-count-%s' "${failed:-0}"
        return 0
    fi
    if [ "$passed" -lt "$floor" ]; then
        printf '1 fail below-floor-%s-of-%s' "$passed" "$floor"
        return 0
    fi
    printf '0 pass ok'
}

# --- modes -----------------------------------------------------------------
print_config() {
    local cargo sysroot
    cargo="$(resolve_cargo || true)"
    sysroot="$(resolve_sysroot || true)"
    say "repo=$REPO_ROOT"
    say "cargo=${cargo:-<not found>}"
    say "toolchain=+$TOOLCHAIN"
    say "miri_flags=$MIRI_FLAGS"
    say "sysroot=${sysroot:-<unset: cargo-miri will run its own setup>}"
    say "filters=${FILTERS[*]}"
    say "min_passed=$MIN_PASSED"
}

self_test() {
    local tmp log code verdict reason failures=0 checks=0 expect got
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/miri-self-test.XXXXXX")" || {
        say 'miri-pure-core: verdict=inconclusive reason=no-temp-dir'
        exit 2
    }
    # shellcheck disable=SC2064
    trap "rm -rf '$tmp'" EXIT

    say 'miri-pure-core: self-test — judge() on synthetic logs'

    expect_case() {
        local name="$1" logfile="$2" rc="$3" want="$4"
        checks=$((checks + 1))
        got="$(judge "$logfile" "$rc")"
        if [ "$got" = "$want" ]; then
            say "  ok   $name -> $got"
        else
            say "  FAIL $name -> got '$got', want '$want'"
            failures=$((failures + 1))
        fi
    }

    printf 'error: Undefined Behavior: memory access failed\n' >"$tmp/ub.log"
    expect_case 'undefined behaviour is a failure' "$tmp/ub.log" 1 '1 ub undefined-behavior'

    printf 'test result: FAILED. 33 passed; 1 failed; 0 ignored\n' >"$tmp/failed.log"
    expect_case 'libtest failure is a failure' "$tmp/failed.log" 1 '1 fail test-failed'

    printf 'test result: ok. 34 passed; 0 failed; 0 ignored\n' >"$tmp/ok.log"
    expect_case 'floor met is a pass' "$tmp/ok.log" 0 '0 pass ok'

    printf 'test result: ok. 16 passed; 0 failed; 0 ignored\n' >"$tmp/floor.log"
    expect_case 'below floor is a failure' "$tmp/floor.log" 0 '1 fail below-floor-16-of-34'

    # The control this script exists for: an empty log with a zero exit code.
    # Treating that as a pass is how a gate stops measuring anything.
    : >"$tmp/empty.log"
    expect_case 'empty log is not a pass' "$tmp/empty.log" 0 '2 inconclusive no-test-result-line'

    # A non-zero exit must never be laundered into a pass by a stray summary line.
    expect_case 'rc!=0 with a happy line is inconclusive' "$tmp/ok.log" 1 '2 inconclusive exit-rc1'

    printf 'error: no such command: `miri`\n' >"$tmp/nomiri.log"
    expect_case 'missing miri is inconclusive' "$tmp/nomiri.log" 101 '2 inconclusive exit-rc101'

    say 'miri-pure-core: self-test — real Miri on throwaway crates'
    local cargo sysroot
    cargo="$(resolve_cargo)" || {
        say 'miri-pure-core: verdict=inconclusive reason=cargo-not-found'
        exit 2
    }
    sysroot="$(resolve_sysroot || true)"
    [ -n "$sysroot" ] && export MIRI_SYSROOT="$sysroot"
    if ! "$cargo" "+$TOOLCHAIN" miri --version >"$tmp/version.log" 2>&1; then
        say 'miri-pure-core: verdict=inconclusive reason=miri-not-installed'
        sed -n '1,5p' "$tmp/version.log"
        say "  remediation: rustup +$TOOLCHAIN component add miri && $cargo +$TOOLCHAIN miri setup"
        exit 2
    fi
    say "  miri: $(tail -1 "$tmp/version.log")"

    # Positive control: a clean crate must come back green, otherwise a broken
    # harness would make the UB control below pass for the wrong reason.
    checks=$((checks + 1))
    "$cargo" new --lib --vcs none --name miri_clean "$tmp/clean" >/dev/null 2>&1
    cat >>"$tmp/clean/src/lib.rs" <<'RS'
#[cfg(test)]
mod t {
    #[test]
    fn clean() {
        let mut v = vec![1u8, 2, 3];
        v.push(4);
        assert_eq!(v.iter().sum::<u8>(), 10);
    }
}
RS
    ( cd "$tmp/clean" && MIRIFLAGS="$MIRI_FLAGS" "$cargo" "+$TOOLCHAIN" miri test --lib ) \
        >"$tmp/clean.log" 2>&1
    code=$?
    got="$(judge "$tmp/clean.log" "$code" 1)"
    if [ "${got%% *}" = '0' ]; then
        say "  ok   clean crate -> $got"
    else
        say "  FAIL clean crate -> $got (a Miri that cannot go green cannot be trusted to go red)"
        sed -n '$p' "$tmp/clean.log"
        failures=$((failures + 1))
    fi

    # Negative control: real UB must be reported. `get_unchecked` past the end is
    # deliberate — Miri catches it deterministically under tree borrows.
    checks=$((checks + 1))
    "$cargo" new --lib --vcs none --name miri_ub "$tmp/ub" >/dev/null 2>&1
    cat >>"$tmp/ub/src/lib.rs" <<'RS'
#[cfg(test)]
mod t {
    #[test]
    fn ub() {
        let bytes = [1u8, 2, 3];
        let bad = unsafe { *bytes.as_ptr().add(3) };
        assert_eq!(bad, 0);
    }
}
RS
    ( cd "$tmp/ub" && MIRIFLAGS="$MIRI_FLAGS" "$cargo" "+$TOOLCHAIN" miri test --lib ) \
        >"$tmp/ub-run.log" 2>&1
    code=$?
    got="$(judge "$tmp/ub-run.log" "$code" 1)"
    if [ "${got%% *}" = '1' ] && case "$got" in *undefined-behavior*) true ;; *) false ;; esac; then
        say "  ok   injected UB -> $got"
    else
        say "  FAIL injected UB -> $got (expected '1 ub undefined-behavior')"
        failures=$((failures + 1))
    fi

    checks=$((checks + 1))
    if scan_ub_holes >"$tmp/holes.log" 2>&1; then
        say '  ok   no cfg(miri) escape holes'
    else
        say '  FAIL cfg(miri) escape holes present'
        sed -n '1,5p' "$tmp/holes.log"
        failures=$((failures + 1))
    fi

    if [ "$failures" -ne 0 ]; then
        say "miri-pure-core: verdict=fail reason=self-test checks=$checks failures=$failures"
        exit 1
    fi
    say "miri-pure-core: verdict=pass reason=self-test checks=$checks failures=0"
    exit 0
}

# --- main ------------------------------------------------------------------
case "${1:-}" in
--print)
    print_config
    exit 0
    ;;
--self-test)
    self_test
    ;;
'')
    ;;
*)
    say "miri-pure-core: verdict=inconclusive reason=unknown-argument-$1"
    exit 2
    ;;
esac

cargo_bin="$(resolve_cargo)" || {
    say 'miri-pure-core: verdict=inconclusive reason=cargo-not-found'
    say '  probed: $CARGO, PATH, ~/.cargo/bin, /opt/homebrew/bin, /usr/local/bin'
    exit 2
}

if [ -n "$(resolve_sysroot || true)" ]; then
    export MIRI_SYSROOT="$(resolve_sysroot)"
else
    say 'miri-pure-core: note: no prebuilt Miri sysroot found; cargo-miri will build one (needs rust-src)'
fi
export MIRIFLAGS="$MIRI_FLAGS"

if ! "$cargo_bin" "+$TOOLCHAIN" miri --version >/dev/null 2>&1; then
    say 'miri-pure-core: verdict=inconclusive reason=miri-not-installed'
    say "  remediation: rustup +$TOOLCHAIN component add miri && $cargo_bin +$TOOLCHAIN miri setup"
    exit 2
fi

if ! scan_ub_holes >/tmp/miri-pure-core-holes.$$ 2>&1; then
    say 'miri-pure-core: verdict=fail reason=cfg-miri-escape-hole'
    say '  a #[cfg(miri)] branch means the gate proves a different program than the one that ships'
    sed -n '1,10p' /tmp/miri-pure-core-holes.$$
    rm -f /tmp/miri-pure-core-holes.$$
    exit 1
fi
rm -f /tmp/miri-pure-core-holes.$$

log="$(mktemp "${TMPDIR:-/tmp}/miri-pure-core.XXXXXX")"
trap 'rm -f "$log"' EXIT

say "miri-pure-core: running filters: ${FILTERS[*]} (min_passed=$MIN_PASSED, flags=$MIRIFLAGS)"
( cd "$REPO_ROOT" && "$cargo_bin" "+$TOOLCHAIN" miri test --lib -- "${FILTERS[@]}" ) >"$log" 2>&1
rc=$?

read -r code verdict reason <<<"$(judge "$log" "$rc")"

case "$code" in
0)
    passed="$(sed -n 's/^test result: .* \([0-9][0-9]*\) passed.*/\1/p' "$log" | tail -1)"
    say "miri-pure-core: verdict=pass passed=$passed floor=$MIN_PASSED"
    ;;
1)
    say "miri-pure-core: verdict=fail reason=$verdict/$reason"
    grep -E 'Undefined Behavior|^test result|panicked at|^error' "$log" | head -20
    ;;
*)
    say "miri-pure-core: verdict=inconclusive reason=$verdict/$reason"
    say '  this is not a pass: the gate could not judge the pure core'
    tail -20 "$log"
    ;;
esac
exit "$code"
