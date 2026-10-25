#!/usr/bin/env bash
# Builds Zell programs under ThreadSanitizer and runs them: the runtime of
# `make -C runtime tsan-lib` (build/tsan/libzell_rt.a) and the program
# instrumented by the compiler (ZELL_SANITIZE=thread, compiler/src/build.rs),
# so TSan sees both the C runtime's and the generated code's accesses.
#
#   scripts/tsan-run.sh <file.zl | dir/main.zl>...
#
# Environment: ZELL (the compiler, default target/release/zell), CC (clang),
# SCALE (passed to the programs, e.g. SCALE=small for the benchmarks),
# TSAN_OUT (log directory, default build/tsan-run under the repository).
# Every report is logged; the summary deduplicates them by the top two
# frames of both accesses. Exits 1 if any program raced or failed.
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
zell=${ZELL:-$root/target/release/zell}
out=${TSAN_OUT:-$root/build/tsan-run}
mkdir -p "$out"
make -s -C "$root/runtime" CC="${CC:-clang}" tsan-lib >/dev/null || exit 1
status=0
for src in "$@"; do
  name=$(echo "$src" | sed 's|^\./||; s|/main\.zl$||; s|\.zl$||; s|/|_|g')
  exe="$out/$name"
  if ! ZELL_SANITIZE=thread ZELL_RUNTIME="$root/runtime/build/tsan" "$zell" build "$src" -o "$exe" >"$out/$name.build" 2>&1; then
    if grep -q '^// error' "$src"; then echo "skip $src (must not compile)"; continue; fi
    echo "BUILD FAILED $src (see $out/$name.build)"; status=1; continue
  fi
  rm -f "$out/$name.tsan".*
  # exit codes of the programs themselves (error-exit specs) are not failures;
  # only TSan's exitcode (66) is.
  TSAN_OPTIONS="exitcode=66 second_deadlock_stack=1 history_size=4 suppressions=$root/runtime/tests/tsan.supp log_path=$out/$name.tsan ${TSAN_OPTIONS:-}" \
    "$exe" >/dev/null 2>"$out/$name.stderr" </dev/null
  rc=$?
  n=$(cat "$out/$name.tsan".* 2>/dev/null | grep -c '^WARNING: ThreadSanitizer' || true)
  if [ "$rc" = 66 ] || [ "$n" != 0 ]; then
    echo "RACES $src: $n report(s)"; status=1
  else
    echo "ok $src"
  fi
done
# Distinct reports: kind | top two frames of the first access | of the second.
cat "$out"/*.tsan.* 2>/dev/null | awk '
  /^WARNING: ThreadSanitizer:/ { if (key != "") print key; kind=$0; sub(/.*ThreadSanitizer: /,"",kind); sub(/ \(pid.*/,"",kind); key=kind; side=0; fr=0; next }
  /^  (Write|Read|Atomic|Previous|Mutex|Location|Thread)/ { side++; fr=0; next }
  /^    #[01] / && side>=1 && side<=2 && fr<2 { f=$2; loc=$3; sub(/.*\//,"",loc); key=key " | " f " " loc; fr++ }
  /^SUMMARY:/ { if (key != "") print key; key="" }
' | sort | uniq -c | sort -rn > "$out/summary.txt"
if [ -s "$out/summary.txt" ]; then
  echo "distinct reports ($out/summary.txt):"; cat "$out/summary.txt"
fi
exit $status
