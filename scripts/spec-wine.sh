#!/usr/bin/env bash
# spec-wine.sh - cross-builds spec programs for windows-amd64 and runs them
# under wine, checking them like compiler/tests/spec.rs does: stdout against
# the .out file, `// exit:`, `// stderr:` lines (substrings, in order),
# `// build-flags:`, `// run-flags:` (passed to the build) and `// env:`
# (`K=` removes K); ZELL_NO_BACKTRACE=1 is set like in the spec runner.
# `// os:` lines are ignored: the point is to run them on Windows.
#
#   scripts/spec-wine.sh                      # tests/spec/panics/*.zl
#   scripts/spec-wine.sh tests/spec/x/y.zl ...
#
# Needs wine, a built compiler (target/release/zell or $ZELL) and the
# Windows runtime (make -C runtime CROSS=x86_64-w64-mingw32).
set -u
cd "$(dirname "$0")/.."
ZELL=${ZELL:-target/release/zell}
command -v wine >/dev/null || { echo "spec-wine: wine not found"; exit 2; }
[ -x "$ZELL" ] || { echo "spec-wine: no compiler at $ZELL"; exit 2; }
export ZELL_NO_BACKTRACE=1
files=("$@")
[ ${#files[@]} -eq 0 ] && files=(tests/spec/panics/*.zl)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
pass=0 fail=0
for f in "${files[@]}"; do
    n=$(basename "$f" .zl)
    want_exit=0 flags=() envs=() unsets=() stderr_want=()
    while IFS= read -r line; do
        case "$line" in
            "// exit: "*) want_exit=${line#// exit: } ;;
            "// build-flags: "*) read -r -a a <<<"${line#// build-flags: }"; flags+=("${a[@]}") ;;
            "// run-flags: "*) read -r -a a <<<"${line#// run-flags: }"; flags+=("${a[@]}") ;;
            "// env: "*) e=${line#// env: }; if [ -z "${e#*=}" ]; then unsets+=(-u "${e%%=*}"); else envs+=("$e"); fi ;;
            "// stderr: "*) stderr_want+=("${line#// stderr: }") ;;
            //*) ;;
            *) break ;;
        esac
    done <"$f"
    exe="$tmp/$n.exe"
    if ! timeout 150 "$ZELL" build --target windows-amd64 ${flags[@]+"${flags[@]}"} "$f" -o "$exe" >"$tmp/$n.build" 2>&1; then
        echo "FAIL $f (build)"; sed 's/^/    /' "$tmp/$n.build" | head -20; fail=$((fail + 1)); continue
    fi
    env ${unsets[@]+"${unsets[@]}"} WINEDEBUG=-all ${envs[@]+"${envs[@]}"} timeout 60 wine "$exe" >"$tmp/$n.stdout" 2>"$tmp/$n.stderr" </dev/null
    code=$?
    tr -d '\r' <"$tmp/$n.stdout" >"$tmp/$n.out"
    tr -d '\r' <"$tmp/$n.stderr" >"$tmp/$n.err"
    why=""
    [ "$code" = "$want_exit" ] || why="exit $code, want $want_exit"
    out="${f%.zl}.out"
    if [ -z "$why" ] && [ -f "$out" ] && ! cmp -s "$out" "$tmp/$n.out"; then why="stdout differs"; fi
    if [ -z "$why" ]; then
        rest=$(cat "$tmp/$n.err")
        for s in ${stderr_want[@]+"${stderr_want[@]}"}; do
            case "$rest" in
                *"$s"*) rest=${rest#*"$s"} ;;
                *) why="stderr lacks: $s"; break ;;
            esac
        done
    fi
    if [ -z "$why" ]; then
        echo "ok   $f"; pass=$((pass + 1))
    else
        echo "FAIL $f ($why)"; fail=$((fail + 1))
        [ -f "$out" ] && diff "$out" "$tmp/$n.out" | head -10 | sed 's/^/    /'
        head -20 "$tmp/$n.err" | sed 's/^/    stderr: /'
    fi
done
echo "spec-wine: $pass passed, $fail failed"
[ "$fail" = 0 ]
