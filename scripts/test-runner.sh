#!/bin/sh
# Cargo's test runner in CI (CARGO_TARGET_<triple>_RUNNER in .github/workflows/ci.yml): starts each
# test binary with libtest's per-test times, which the test budget reads (NIT-522). RUSTC_BOOTSTRAP
# reaches only the test binary, so the build is the same as without it. In a build lane,
# `nitely test-runner` does this.
bin="$1"
shift
case " $* " in
  *" --list "* | *" --bench "* | *" --format"* | *" --report-time "* | *" -q "* | *" --quiet "*) exec "$bin" "$@" ;;
esac
RUSTC_BOOTSTRAP=1 exec "$bin" "$@" -Z unstable-options --report-time
