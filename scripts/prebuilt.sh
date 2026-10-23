# Shared by bundle-targets.sh and build-lld.sh (sourced, not run): the
# download side of the `toolchain-inputs` release (docs/PLATFORMS.md,
# "Дистрибутив"; produced by .github/workflows/inputs.yml).
#
# prebuilt_sha256 FILE: hex sha256 of FILE
# prebuilt_fetch URL_DIR ASSET DEST_DIR: downloads URL_DIR/ASSET and
#   URL_DIR/ASSET.sha256 into DEST_DIR and verifies the first against the
#   second. Returns 0 (both in DEST_DIR, verified), 3 (no such asset: HTTP 404
#   or a missing file:// path) or 1 (any other failure, including a checksum
#   mismatch: never silently falls back on a corrupt download). URL_DIR is
#   http(s):// or file:// (tests, mirrors).
prebuilt_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

prebuilt_get() { # URL FILE: 0 ok, 3 missing, 1 error
    rm -f "$2"
    code=$(curl -sSL --retry 3 -o "$2" -w '%{http_code}' "$1" 2>/dev/null) && rc=0 || rc=$?
    case "$rc:$code" in
        0:200|0:000) return 0 ;;                     # 000: file:// has no HTTP status
        0:404|22:*|37:*) rm -f "$2"; return 3 ;;     # 37: file:// path does not exist
        *) echo "prebuilt: $1: curl exit $rc, HTTP $code" >&2; rm -f "$2"; return 1 ;;
    esac
}

prebuilt_fetch() {
    pf_url=${1%/}; pf_asset=$2; pf_dest=$3
    mkdir -p "$pf_dest"
    prebuilt_get "$pf_url/$pf_asset.sha256" "$pf_dest/$pf_asset.sha256" && pf_rc=0 || pf_rc=$?
    [ "$pf_rc" = 0 ] || return "$pf_rc"
    prebuilt_get "$pf_url/$pf_asset" "$pf_dest/$pf_asset" && pf_rc=0 || pf_rc=$?
    if [ "$pf_rc" != 0 ]; then
        [ "$pf_rc" = 3 ] && echo "prebuilt: $pf_asset.sha256 exists but $pf_asset does not" >&2
        return 1
    fi
    pf_want=$(cut -d' ' -f1 "$pf_dest/$pf_asset.sha256" | head -1)
    pf_got=$(prebuilt_sha256 "$pf_dest/$pf_asset")
    if [ -z "$pf_want" ] || [ "$pf_got" != "$pf_want" ]; then
        echo "prebuilt: $pf_asset: sha256 $pf_got, expected ${pf_want:-<empty>} (from $pf_asset.sha256)" >&2
        rm -f "$pf_dest/$pf_asset"
        return 1
    fi
    return 0
}
