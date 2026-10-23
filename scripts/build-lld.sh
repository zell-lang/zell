#!/bin/sh
# Builds bin/lld of the self-contained toolchain (docs/PLATFORMS.md,
# "Дистрибутив"): one multi-flavour lld (ELF, COFF/MinGW, Mach-O) that
# depends on nothing but the OS's C library, from LLVM 18.
#
#   scripts/build-lld.sh [--out DIR] [--prebuilt [URL]] [--print-key]
#                                           -> DIR/lld (default build/lld/)
#
# --prebuilt [URL]: first try the output of .github/workflows/inputs.yml: the
#   release `toolchain-inputs` (URL default
#   https://github.com/zell-lang/zell/releases/download/toolchain-inputs, or
#   $ZELL_INPUTS_URL; http(s):// or file://) holds lld-18-<host>-<key>.tar.xz
#   (+ .sha256; host: linux-amd64, linux-arm64, macos-arm64, macos-amd64,
#   windows-amd64, as in package-toolchain.sh) with just lld[.exe]. It is
#   verified (a mismatch is an error), unpacked into DIR and checked by running
#   it; a missing asset (404: the key changed and inputs.yml was not re-run)
#   falls back to the steps below, slower.
# --print-key: print <key> and exit: the first 16 hex digits of the sha256 of
#   scripts/build-lld.sh followed by scripts/lld/lld-main.cpp.
#
# How, in order:
#   1. $LLD_BINARY: an existing self-contained lld to copy (Windows: lld.exe of
#      the official clang+llvm-18.1.8-x86_64-pc-windows-msvc tarball, linked
#      with the MSVC runtime that package-toolchain.sh ships next to it).
#   2. The static lld and LLVM libraries of an installed LLVM 18 ($LLVM_CONFIG,
#      llvm-config-18 or llvm-config: Ubuntu's llvm-18-dev + liblld-18-dev,
#      Homebrew's llvm@18): scripts/lld/lld-main.cpp (lld's own driver
#      reduced to its four flavours) linked statically, libstdc++/zlib/zstd/
#      terminfo included, the backends other than X86 and AArch64 left out.
#   3. Otherwise lld built from the llvm-project 18.1.8 sources (cmake +
#      ninja, X86 and AArch64 only, no zlib/zstd/libxml2/terminfo): slow
#      (30-60 minutes), meant for a cached CI step on hosts without (2).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/lld
prebuilt=no
print_key=no
inputs_url=${ZELL_INPUTS_URL:-https://github.com/zell-lang/zell/releases/download/toolchain-inputs}
while [ $# -gt 0 ]; do
    case "$1" in
        --out) shift; out=$1 ;;
        --print-key) print_key=yes ;;
        --prebuilt)
            prebuilt=yes
            case "${2:-}" in http://*|https://*|file://*) shift; inputs_url=$1 ;; esac ;;
        *) echo "build-lld.sh: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done
lld_key() {
    cat "$root/scripts/build-lld.sh" "$root/scripts/lld/lld-main.cpp" | { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; } | cut -c1-16
}
if [ "$print_key" = yes ]; then lld_key; exit 0; fi
mkdir -p "$out"
exe=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) exe=.exe ;; esac
dst=$out/lld$exe

if [ "$prebuilt" = yes ]; then
    . "$root/scripts/prebuilt.sh"
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64) host=linux-amd64 ;;
        Linux-aarch64) host=linux-arm64 ;;
        Darwin-arm64) host=macos-arm64 ;;
        Darwin-x86_64) host=macos-amd64 ;;
        MINGW*-x86_64|MSYS*-x86_64) host=windows-amd64 ;;
        *) host="" ;;
    esac
    asset=lld-18-$host-$(lld_key).tar.xz
    pdir=$root/build/prebuilt-lld
    rm -rf "$pdir"; mkdir -p "$pdir"
    if [ -z "$host" ]; then
        echo "build-lld.sh: no prebuilt lld for $(uname -s)-$(uname -m); building" >&2
    else
        prebuilt_fetch "$inputs_url" "$asset" "$pdir" && rc=0 || rc=$?
        case "$rc" in
            0)
                tar xJf "$pdir/$asset" -C "$out" "lld$exe" || { echo "build-lld.sh: $asset: cannot unpack" >&2; exit 1; }
                rm -rf "$pdir"
                chmod +x "$dst"
                "$dst" -flavor gnu --version
                echo "build-lld.sh: prebuilt $asset" >&2
                exit 0 ;;
            3) echo "build-lld.sh: no prebuilt $asset in $inputs_url (key changed: run inputs.yml after changing build-lld.sh or lld-main.cpp); building lld here, slower" >&2 ;;
            *) echo "build-lld.sh: prebuilt $asset failed (above)" >&2; exit 1 ;;
        esac
    fi
    rm -rf "$pdir"
fi

if [ -n "${LLD_BINARY:-}" ]; then
    cp "$LLD_BINARY" "$dst"
    "$dst" -flavor gnu --version
    exit 0
fi

LC=${LLVM_CONFIG:-$(command -v llvm-config-18 || command -v llvm-config || true)}
if [ -n "$LC" ] && [ -f "$("$LC" --libdir)/liblldELF.a" ]; then
    libdir=$("$LC" --libdir)
    CXX=${CXX:-$("$LC" --bindir)/clang++}
    [ -x "$CXX" ] || CXX=$(command -v clang++-18 || command -v clang++ || command -v c++)
    stubs=""
    for t in $("$LC" --targets-built); do
        case "$t" in X86|AArch64) ;; *) stubs="$stubs ZELL_STUB_TARGET($t)" ;; esac
    done
    case "$(uname -s)" in
        Darwin)
            # system libz/libc++/libcurses only (Apple's clang++: Homebrew's
            # would add its own libc++/libunwind dylibs); Homebrew's zstd
            # linked in statically
            CXX=${LLD_CXX:-/usr/bin/clang++}
            zstd=$(brew --prefix zstd 2>/dev/null)/lib/libzstd.a
            [ -f "$zstd" ] || zstd=-lzstd
            sys="$zstd -lz -lcurses -lm"
            static="" ;;
        *)
            sys="-Wl,-Bstatic -lz -lzstd -ltinfo -Wl,-Bdynamic -lrt -ldl -lm -lpthread"
            static="-static-libstdc++ -static-libgcc -Wl,--gc-sections -Wl,-O1" ;;
    esac
    # static archives by full path, no -L of LLVM's lib directory: Homebrew's
    # holds a libc++.dylib (re-exporting its libunwind) that would shadow the OS's
    # shellcheck disable=SC2086
    "$CXX" -O2 -std=c++17 $("$LC" --cxxflags) "-DZELL_LLD_STUB_TARGETS=$stubs" \
        "$root/scripts/lld/lld-main.cpp" -o "$dst" \
        "$libdir/liblldMinGW.a" "$libdir/liblldCOFF.a" "$libdir/liblldELF.a" "$libdir/liblldMachO.a" "$libdir/liblldCommon.a" \
        $("$LC" --link-static --libfiles all) $static $sys
    strip "$dst" 2>/dev/null || true
else
    # 3. from source
    v=18.1.8
    sha=0b58557a6d32ceee97c8d533a59b9212d87e0fc4d2833924eb6c611247db2f2a
    src=$root/build/lld-src
    mkdir -p "$src"
    tarball=$src/llvm-project-$v.src.tar.xz
    if [ ! -f "$tarball" ]; then
        curl -fsSL --retry 3 -o "$tarball.part" "https://github.com/llvm/llvm-project/releases/download/llvmorg-$v/llvm-project-$v.src.tar.xz"
        mv "$tarball.part" "$tarball"
    fi
    got=$( (sha256sum "$tarball" 2>/dev/null || shasum -a 256 "$tarball") | cut -d' ' -f1)
    [ "$got" = "$sha" ] || { echo "build-lld.sh: $tarball: sha256 $got, expected $sha" >&2; exit 1; }
    [ -d "$src/llvm-project-$v.src" ] || tar xJf "$tarball" -C "$src" "llvm-project-$v.src/llvm" "llvm-project-$v.src/lld" "llvm-project-$v.src/cmake" "llvm-project-$v.src/libunwind/include" "llvm-project-$v.src/third-party"
    b=$src/build
    cmake -G Ninja -S "$src/llvm-project-$v.src/llvm" -B "$b" -DCMAKE_BUILD_TYPE=Release \
        -DLLVM_ENABLE_PROJECTS=lld -DLLVM_TARGETS_TO_BUILD="X86;AArch64" \
        -DLLVM_ENABLE_ZLIB=OFF -DLLVM_ENABLE_ZSTD=OFF -DLLVM_ENABLE_LIBXML2=OFF -DLLVM_ENABLE_TERMINFO=OFF \
        -DLLVM_INCLUDE_TESTS=OFF -DLLVM_INCLUDE_BENCHMARKS=OFF -DLLVM_INCLUDE_EXAMPLES=OFF -DLLVM_ENABLE_BINDINGS=OFF \
        -DLLVM_BUILD_TOOLS=OFF -DLLVM_STATIC_LINK_CXX_STDLIB=ON
    ninja -C "$b" lld
    cp "$b/bin/lld$exe" "$dst"
    strip "$dst" 2>/dev/null || true
fi
"$dst" -flavor gnu --version
ls -l "$dst"
# self-contained: no library outside the OS (Homebrew, /usr/local, a build tree)
if command -v otool >/dev/null 2>&1; then
    if otool -L "$dst" | tail -n +2 | grep -v -e '^[[:space:]]*/usr/lib/' -e '^[[:space:]]*/System/'; then
        echo "build-lld.sh: $dst depends on libraries outside the OS (above)" >&2; exit 1
    fi
elif command -v ldd >/dev/null 2>&1 && [ -z "$exe" ]; then
    if ldd "$dst" | grep -v -e 'linux-vdso' -e 'ld-linux' -e '/libc\.so' -e '/libm\.so' -e '/libdl\.so' -e '/librt\.so' -e '/libpthread\.so'; then
        echo "build-lld.sh: $dst depends on libraries outside glibc (above)" >&2; exit 1
    fi
fi
