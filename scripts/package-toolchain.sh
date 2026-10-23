#!/bin/sh
# Packages a self-contained Zell toolchain for this machine (docs/PLATFORMS.md,
# "Дистрибутив"): unpack it and build, run and cross-compile for every
# tier-1/2 target with nothing else installed (no clang, libc headers,
# mingw-w64 or SDK).
#
#   dist/zell-<version>-<host>.tar.xz      unpacks to zell-<version>-<host>/ (xz: about
#                                           30 MB instead of 70 MB with gzip; .tar.gz when
#                                           this machine has no xz):
#       bin/zell, bin/zell-lsp, bin/zell-mcp  the compiler, language server, MCP server
#       bin/lld (+ ld.lld, ld64.lld, lld-link links; lld.exe on Windows)
#                                           the linker (scripts/build-lld.sh)
#       bin/*.dll                           Windows: the MSVC runtime the binaries use
#       std/                                standard library sources
#       targets/<triple>/                   link inputs of every target
#                                           (scripts/bundle-targets.sh: runtime,
#                                           compiler-rt, crt objects, C library stubs)
#       tools/                              lldb/gdb formatters, VS Code client
#       THIRD_PARTY.md, README.md, VERSION
#   dist/zell-runtime-<version>-<triple>.tar.gz
#                                           one per target, unpacking to <triple>/ (the
#                                           same directory): what `zell target add`
#                                           downloads into ~/.zell/targets
#   *.sha256 next to every archive
#
# Usage: scripts/package-toolchain.sh [--no-build] [--out dir] [--targets dir] [--lld file] [--version V]
#   --no-build     use the existing target/release binaries
#   --out dir      output directory (default dist/)
#   --targets dir  link inputs (default build/targets/, from scripts/bundle-targets.sh);
#                  without it only the host runtime is packaged and linking needs clang
#   --lld file     the linker (default build/lld/lld, from scripts/build-lld.sh)
#   --version V    name archives and VERSION by V instead of the Cargo.toml version
#                  (or $ZELL_VERSION): a release candidate is 0.1.0-rc.1, giving
#                  zell-0.1.0-rc.1-<host>.tar.xz (release.yml passes the tag without "v")
# The archives are what a release uploads; install.sh / install.ps1 install
# them, and ZELL_TARGETS_URL may point at a directory of runtime archives.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/dist"
build=yes
targets="$root/build/targets"
lld=""
version=${ZELL_VERSION:-}
while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) build=no ;;
        --out) shift; out=$1 ;;
        --targets) shift; targets=$1 ;;
        --lld) shift; lld=$1 ;;
        --version) shift; version=$1 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
    shift
done
[ -n "$version" ] || version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) host=linux-amd64; triple=x86_64-unknown-linux-gnu ;;
    Linux-aarch64) host=linux-arm64; triple=aarch64-unknown-linux-gnu ;;
    Darwin-arm64) host=macos-arm64; triple=aarch64-apple-darwin ;;
    Darwin-x86_64) host=macos-amd64; triple=x86_64-apple-darwin ;;
    MINGW*-x86_64|MSYS*-x86_64) host=windows-amd64; triple=x86_64-pc-windows-gnu ;;
    *) echo "unsupported host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
exe=""
[ "$host" = windows-amd64 ] && exe=.exe
[ -n "$lld" ] || lld="$root/build/lld/lld$exe"
if [ "$build" = yes ]; then
    (cd "$root" && cargo build --release --bin zell --bin zell-lsp --bin zell-mcp)
    make -C "$root/runtime" -j4
fi
for f in "$root/target/release/zell$exe" "$root/target/release/zell-lsp$exe" "$root/target/release/zell-mcp$exe"; do
    [ -f "$f" ] || { echo "missing $f (run without --no-build)" >&2; exit 1; }
done
bundled=no
if [ -f "$targets/$triple/BUNDLE" ]; then
    bundled=yes
    [ -f "$lld" ] || { echo "missing the linker $lld (scripts/build-lld.sh, or --lld)" >&2; exit 1; }
else
    echo "warning: no link inputs in $targets (scripts/bundle-targets.sh): packaging the host runtime only; linking will need clang" >&2
    rt="$root/runtime/libzell_rt.a"
    [ "$host" = windows-amd64 ] && rt="$root/runtime/build/x86_64-pc-windows-gnu/libzell_rt.a"
    [ -f "$rt" ] || { echo "missing $rt" >&2; exit 1; }
fi
name="zell-$version-$host"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
d="$stage/$name"
mkdir -p "$d/bin" "$d/targets" "$d/tools"
cp "$root/target/release/zell$exe" "$root/target/release/zell-lsp$exe" "$root/target/release/zell-mcp$exe" "$d/bin/"
strip "$d/bin/zell$exe" "$d/bin/zell-lsp$exe" "$d/bin/zell-mcp$exe" 2>/dev/null || true
if [ "$bundled" = yes ]; then
    cp "$lld" "$d/bin/lld$exe"
    strip "$d/bin/lld$exe" 2>/dev/null || true
    if [ -z "$exe" ]; then
        # flavour names (ld.lld is also what the parallel code generator's
        # partial links run); zell itself calls `lld -flavor ...`
        for f in ld.lld ld64.lld lld-link; do ln -s lld "$d/bin/$f"; done
    fi
    for t in "$targets"/*/; do
        t=$(basename "$t")
        [ -f "$targets/$t/BUNDLE" ] || continue
        cp -R "$targets/$t" "$d/targets/$t"
    done
    [ -f "$targets/SOURCES.sha256" ] && cp "$targets/SOURCES.sha256" "$d/targets/"
else
    mkdir -p "$d/targets/$triple"
    cp "$rt" "$d/targets/$triple/libzell_rt.a"
fi
if [ "$host" = windows-amd64 ]; then
    # zell.exe & co. are built against the official LLVM MSVC libraries (/MD):
    # the Visual C++ runtime goes next to them (Microsoft's redistributable
    # files; THIRD_PARTY.md)
    for dll in vcruntime140.dll vcruntime140_1.dll msvcp140.dll; do
        src=""
        for dir in "${VCToolsRedistDir:-}" /c/Windows/System32 "${SYSTEMROOT:-C:/Windows}/System32"; do
            [ -n "$dir" ] || continue
            found=$(find "$dir" -maxdepth 4 -iname "$dll" -path '*x64*' 2>/dev/null | head -1)
            [ -n "$found" ] || found=$(ls "$dir/$dll" 2>/dev/null | head -1)
            [ -n "$found" ] && { src=$found; break; }
        done
        [ -n "$src" ] || { echo "missing $dll (the Visual C++ redistributable)" >&2; exit 1; }
        cp "$src" "$d/bin/$dll"
    done
fi
cp -R "$root/std" "$d/std"
cp -R "$root/tools/." "$d/tools/"
if [ -d "$root/editors/vscode" ]; then
    mkdir -p "$d/tools/vscode"
    (cd "$root/editors/vscode" && tar cf - --exclude=node_modules .) | (cd "$d/tools/vscode" && tar xf -)
fi
cp "$root/THIRD_PARTY.md" "$d/THIRD_PARTY.md"
printf '%s\n' "$version" > "$d/VERSION"
if [ "$bundled" = yes ]; then
    linking="Everything needed to link is in this directory: bin/lld and targets/ (see
\`zell doctor\`, \`zell target list\`)."
else
    linking="This archive has no bundled link inputs: linking needs clang on PATH."
fi
cat > "$d/README.md" <<README
# Zell $version for $host

    export PATH="\$PWD/bin:\$PATH"     # or run install.sh, which does this
    zell run hello.zl
    zell build --target windows-amd64 hello.zl     # also linux-arm64, linux-amd64-musl, macos-arm64, ...

$linking
Linux executables need glibc 2.31 or newer; \`--target linux-amd64-musl\` gives
fully static ones. Licences of the bundled parts: THIRD_PARTY.md. See
docs/PLATFORMS.md in the repository.
README
mkdir -p "$out"
out=$(cd "$out" && pwd)
sum() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi; }
gz() { if command -v pigz >/dev/null 2>&1; then pigz -9; else gzip -9; fi; }
if command -v xz >/dev/null 2>&1; then
    arc=$name.tar.xz
    (cd "$stage" && tar cf - "$name" | xz -9 -T0 > "$out/$arc")
else
    arc=$name.tar.gz
    (cd "$stage" && tar cf - "$name" | gz > "$out/$arc")
fi
(cd "$out" && sum "$arc" > "$arc.sha256")
if [ "$host" = windows-amd64 ] && [ "$arc" = "$name.tar.xz" ]; then
    # older Windows 10 tar.exe builds cannot read xz: install.ps1 falls back to this
    (cd "$stage" && tar cf - "$name" | gz > "$out/$name.tar.gz")
    (cd "$out" && sum "$name.tar.gz" > "$name.tar.gz.sha256")
fi
printf '%s\n' "$version" > "$out/VERSION"
echo "$out/$arc ($(du -k "$out/$arc" | cut -f1) KB)"
# runtime archives for `zell target add`: each target's directory as bundled
pack_runtime() { # triple dir
    rd="$stage/rt-$1"
    mkdir -p "$rd"
    cp -R "$2" "$rd/$1"
    (cd "$rd" && tar cf - "$1" | gz > "$out/zell-runtime-$version-$1.tar.gz")
    (cd "$out" && sum "zell-runtime-$version-$1.tar.gz" > "zell-runtime-$version-$1.tar.gz.sha256")
    echo "$out/zell-runtime-$version-$1.tar.gz"
}
for t in "$d"/targets/*/; do
    t=$(basename "$t")
    pack_runtime "$t" "$d/targets/$t"
done
if [ "$bundled" = no ]; then
    # cross-built runtimes of this checkout (make -C runtime CROSS=...)
    for lib in "$root"/runtime/build/*/libzell_rt.a; do
        [ -f "$lib" ] || continue
        t=$(basename "$(dirname "$lib")")
        case "$t" in *-unknown-*|*-pc-*|*-apple-*) ;; *) continue ;; esac   # LLVM triples only (not win64/, aarch64-linux-gnu/ object dirs)
        case "$t" in *stubcheck*|*stubsdk*|bundle) continue ;; esac
        [ -f "$out/zell-runtime-$version-$t.tar.gz" ] && continue
        mkdir -p "$stage/rtsrc-$t"
        cp "$lib" "$stage/rtsrc-$t/libzell_rt.a"
        pack_runtime "$t" "$stage/rtsrc-$t"
    done
fi
