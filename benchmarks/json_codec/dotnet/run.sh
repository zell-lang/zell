#!/bin/sh
# Builds (once) and runs the .NET json_codec benchmark. Env: SCALE, JSON_DOC_FILE, JSON_DOC_BYTES.
set -e
cd "$(dirname "$0")"
unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy
if [ ! -f bin/json_codec.dll ]; then
  dotnet publish -c Release -r linux-x64 --self-contained false -o bin >&2
fi
exec dotnet bin/json_codec.dll
