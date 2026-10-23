#!/bin/sh
# Runs Zell, Go and .NET once each on one JSON_DOC_FILE and prints the
# checksum and phase lines side by side.
# Usage: ZELL=/path/to/zell JSON_DOC_FILE=/path/doc.json ./compare.sh   (SCALE=small works too)
set -e
cd "$(dirname "$0")"
ZELL=${ZELL:-zell}
export JSON_DOC_FILE=${JSON_DOC_FILE:-/tmp/json_codec_doc.json}
tmp=$(mktemp -d)
$ZELL build main.zl --release -o "$tmp/zl" >/dev/null
/usr/local/go/bin/go build -o "$tmp/go" main.go 2>/dev/null || go build -o "$tmp/go" main.go
for lang in zell go dotnet; do
  case $lang in
    zell) "$tmp/zl" >"$tmp/$lang.out" 2>"$tmp/$lang.err" ;;
    go) "$tmp/go" >"$tmp/$lang.out" 2>"$tmp/$lang.err" ;;
    dotnet) ./dotnet/run.sh >"$tmp/$lang.out" 2>"$tmp/$lang.err" ;;
  esac
done
for lang in zell go dotnet; do
  echo "== $lang"
  cat "$tmp/$lang.out"
  grep -E '^(typed|value|messages)' "$tmp/$lang.err" || true
done
if cmp -s "$tmp/zell.out" "$tmp/go.out" && cmp -s "$tmp/go.out" "$tmp/dotnet.out"; then
  echo "checksum lines: identical"
else
  echo "checksum lines: DIFFER"
fi
rm -rf "$tmp"
