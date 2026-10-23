#!/bin/sh
# Assembles the per-target link inputs of a self-contained Zell toolchain
# (docs/PLATFORMS.md, "Дистрибутив"): everything `zell build --target T`
# needs to link with the bundled lld and nothing else installed.
#
#   build/targets/<triple>/
#     BUNDLE                    what this directory is (read by `zell doctor`)
#     libzell_rt.a              the runtime, built here for the target (no debug info)
#     libclang_rt.builtins.a    compiler-rt builtins (__udivti3, __aarch64_ldadd8_acq_rel, ___chkstk_ms...)
#     linux-gnu:  Scrt1.o crt1.o crti.o crtn.o crtbeginS.o crtendS.o libc_nonshared.a (glibc 2.31)
#                 libc.so libm.so libpthread.so libdl.so librt.so libutil.so ld-linux-*.so.*  stubs with
#                 GLIBC_2.x version nodes (abilists of glibc 2.31), libsqlite3.so (stub of 3.45)
#     linux-musl: crt1.o rcrt1.o crti.o crtn.o crtbegin.o crtend.o libc.a (musl, built here)
#     windows:    crt2.o dllcrt2.o libmingw32.a libmingwex.a libmoldname.a libmsvcrt.a + import
#                 libraries (kernel32, ws2_32, ...) from mingw-w64
#     macos:      libSystem.tbd (text stub: what the runtime and generated code import plus the
#                 C/POSIX API), the runtime built against runtime/stub-sdk/macos
#
# Usage: scripts/bundle-targets.sh [--cache DIR] [--out DIR] [--offline] [triple...]
#   --cache DIR  downloads (default build/bundle-cache, or $ZELL_BUNDLE_CACHE); every file
#                is pinned below by URL and sha256 and re-checked on every run
#   --out DIR    output directory (default build/targets)
#   --offline    never download: everything must already be in the cache
#   --check-darwin-tbd  on a Mac: check the libSystem.tbd files in --out against the SDK
#   triple...    a subset of the targets (default: all tier-1/2 targets)
#
# Needs (on the Linux machine that builds the bundle, CI or a developer box):
# clang 18 and the LLVM 18 tools (llvm-ar, llvm-readelf, llvm-strip), ld.lld,
# make, dpkg-deb (or ar + tar with zstd), xz, sha256sum, curl. The output is
# the same on every host: package-toolchain.sh copies it into each archive.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cache=${ZELL_BUNDLE_CACHE:-$root/build/bundle-cache}
out=$root/build/targets
offline=no
targets=""
while [ $# -gt 0 ]; do
    case "$1" in
        --cache) shift; cache=$1 ;;
        --out) shift; out=$1 ;;
        --offline) offline=yes ;;
        --check-darwin-tbd) check_tbd=yes ;;
        -h|--help) sed -n '2,33p' "$0"; exit 0 ;;
        -*) echo "bundle-targets.sh: unknown option $1" >&2; exit 2 ;;
        *) targets="$targets $1" ;;
    esac
    shift
done
[ -n "$targets" ] || targets="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-pc-windows-gnu aarch64-apple-darwin x86_64-apple-darwin"
mkdir -p "$cache" "$out"
cache=$(cd "$cache" && pwd)
out=$(cd "$out" && pwd)
work=$root/build/bundle-work
rm -rf "$work"
mkdir -p "$work"

# --- pinned inputs -----------------------------------------------------------
# Ubuntu archive pools keep release-pocket packages forever, so these URLs are
# stable. glibc 2.31 (Ubuntu 20.04) is the floor of linux-gnu executables:
# they run on any glibc >= 2.31 (Debian 11+, Ubuntu 20.04+, RHEL 9+).
UA=http://archive.ubuntu.com/ubuntu/pool
UP=http://ports.ubuntu.com/ubuntu-ports/pool
GLIBC_VERSION=2.31
MUSL_VERSION=1.2.5
MINGW_VERSION=11.0.1
SQLITE_VERSION=3.45.1
COMPILER_RT_VERSION=18.1.8
MACOS_MIN=12.3   # macOS deployment target (crosslink.rs -platform_version; SDK: sqlite3_changes64)
PINS="
libc6-amd64|$UA/main/g/glibc/libc6_2.31-0ubuntu9_amd64.deb|847a68e5f1ec2cd4e6fc191c5eaf0547445dd5c473c7617c068ba4441f63b292
libc6-dev-amd64|$UA/main/g/glibc/libc6-dev_2.31-0ubuntu9_amd64.deb|adb78f38fb00c76af4384be7a4c5f41da242e05bea6b0483e03b7e0c86738477
linux-libc-dev-amd64|$UA/main/l/linux/linux-libc-dev_5.4.0-26.30_amd64.deb|a7d59420134a8307eb11ef79b68e2b35cadc794a60f82c87f4583e37c763fd01
libc6-arm64|$UP/main/g/glibc/libc6_2.31-0ubuntu9_arm64.deb|783130400addeedde4c32e020d8e6bb612d9c8296eea66c74f2f86cbc938c033
libc6-dev-arm64|$UP/main/g/glibc/libc6-dev_2.31-0ubuntu9_arm64.deb|7396b98f74412c7160c89717c51e50392c7d66c8d66fd7e6795cee2545feddce
linux-libc-dev-arm64|$UP/main/l/linux/linux-libc-dev_5.4.0-26.30_arm64.deb|960d3d3d98ca3363878ac99d602c40ddb5e5f7e586a5f63cdfddcfe77595807f
libsqlite3-0-amd64|$UA/main/s/sqlite3/libsqlite3-0_3.45.1-1ubuntu2_amd64.deb|6ea94c8a822551187cef0085073910eeae0a7525cfec4415171942c7019c68af
libsqlite3-0-arm64|$UP/main/s/sqlite3/libsqlite3-0_3.45.1-1ubuntu2_arm64.deb|b71c08ea650c212b2c8fa5bbcef9957e5e85c8f5c484a85cfb8961a8271bf376
libsqlite3-dev|$UA/main/s/sqlite3/libsqlite3-dev_3.45.1-1ubuntu2_amd64.deb|d23577c43936fedd8c4fa1337a6e960a6e71e94ac164d7a15c46ea96bf21265d
mingw-w64-x86-64-dev|$UA/universe/m/mingw-w64/mingw-w64-x86-64-dev_11.0.1-3build1_all.deb|018480a62907fcca19c6bf2975ae01f5c92c23dee8c3cf7a8355c10bf7d42c6f
mingw-w64-common|$UA/universe/m/mingw-w64/mingw-w64-common_11.0.1-3build1_all.deb|4a42fd30fbc3838c58c929d8a4882682fc12408bb7a0e6cafa913c0f0a57c187
musl-src|$UA/universe/m/musl/musl_1.2.5.orig.tar.gz|a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4
compiler-rt-src|https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.8/compiler-rt-18.1.8.src.tar.xz|e054e99a9c9240720616e927cb52363abbc8b4f1ef0286bad3df79ec8fdf892f
"

# check_darwin_tbd DIR...: on a Mac, every symbol of the bundled
# libSystem.tbd files must be exported by the SDK's libSystem or one of its
# re-exported libraries (usr/lib/system/*.tbd); prints the ones that are not.
check_darwin_tbd() {
    sdk=${SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}
    cat "$sdk/usr/lib/libSystem.B.tbd" "$sdk"/usr/lib/system/*.tbd | tr ',[]' '\n\n\n' | sed -e "s/[' ]//g" -e '/^$/d' | sort -u > "$work/sdk.syms"
    bad=0
    for d in "$@"; do
        if [ -f "$d/libsqlite3.tbd" ]; then
            tr ',[]' '\n\n\n' < "$sdk/usr/lib/libsqlite3.tbd" | sed -e "s/[' ]//g" -e '/^$/d' >> "$work/sdk.syms"
            sort -u -o "$work/sdk.syms" "$work/sdk.syms"
        fi
        cat "$d/libSystem.tbd" "$d"/libsqlite3.tbd 2>/dev/null | sed -n "s/^      '\(.*\)',\{0,1\}$/\1/p" | sort -u > "$work/ours.syms"
        missing=$(comm -23 "$work/ours.syms" "$work/sdk.syms")
        if [ -n "$missing" ]; then
            echo "$d/libSystem.tbd: not in the SDK's libSystem:" $missing >&2
            bad=1
        else
            echo "$d/libSystem.tbd: all $(wc -l < "$work/ours.syms" | tr -d ' ') symbols are in $sdk" >&2
        fi
    done
    return $bad
}

if [ "${check_tbd:-no}" = yes ]; then
    check_darwin_tbd "$out/aarch64-apple-darwin" "$out/x86_64-apple-darwin"
    exit $?
fi

# --- tools -------------------------------------------------------------------
CLANG=${CLANG:-$(command -v clang-18 || command -v clang || true)}
[ -n "$CLANG" ] || { echo "bundle-targets.sh: needs clang 18 (scripts/setup.sh)" >&2; exit 1; }
LLVM_BIN=${LLVM_BIN:-$(dirname "$(readlink -f "$(command -v "$CLANG")")")}
tool() { # name: the LLVM 18 tool next to clang, else on PATH (name-18, name)
    for c in "$LLVM_BIN/$1" "$(command -v "$1-18" || true)" "$(command -v "$1" || true)"; do
        [ -n "$c" ] && [ -x "$c" ] && { echo "$c"; return; }
    done
    echo "bundle-targets.sh: missing $1 (LLVM 18)" >&2; exit 1
}
AR=$(tool llvm-ar)
RANLIB=$(tool llvm-ranlib)
READELF=$(tool llvm-readelf)
NM=$(tool llvm-nm)
STRIP=$(tool llvm-strip)
LLD=$(tool ld.lld)
JOBS=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)
sum() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

# fetch NAME: the cached, checksum-verified file of pin NAME
fetch() {
    line=$(printf '%s\n' "$PINS" | grep "^$1|") || { echo "no pin $1" >&2; exit 1; }
    url=$(echo "$line" | cut -d'|' -f2)
    want=$(echo "$line" | cut -d'|' -f3)
    f="$cache/$(basename "$url")"
    if [ ! -f "$f" ] || [ "$(sum "$f")" != "$want" ]; then
        [ "$offline" = no ] || { echo "bundle-targets.sh: $f missing or wrong in the cache (--offline)" >&2; exit 1; }
        echo "  download $url" >&2
        curl -fsSL --retry 3 -o "$f.part" "$url"
        got=$(sum "$f.part")
        [ "$got" = "$want" ] || { echo "bundle-targets.sh: $url: sha256 $got, expected $want" >&2; rm -f "$f.part"; exit 1; }
        mv "$f.part" "$f"
    fi
    echo "$f"
}

# undeb PIN DIR: unpack a .deb's files into DIR
undeb() {
    f=$(fetch "$1")
    mkdir -p "$2"
    if command -v dpkg-deb >/dev/null 2>&1; then
        dpkg-deb -x "$f" "$2"
    else
        t=$(mktemp -d "$work/deb.XXXXXX")
        (cd "$t" && "$AR" x "$f" && tar xf data.tar.* -C "$2")
        rm -rf "$t"
    fi
}

# --- ELF shared-object stubs (Zig's abilists approach) -------------------------
# elf_stub SRC.so OUT SONAME CLANG_TARGET [keep-unversioned]: a shared object
# with SONAME that defines every default-version (`@@`) dynamic symbol of SRC
# with its type, size and version node, and no code. Linking against it
# records exactly the NEEDED entries and version requirements the real
# library would; at run time the real one is loaded. GLIBC_PRIVATE symbols
# are left out.
elf_stub() {
    src=$1; dst=$2; soname=$3; tgt=$4
    base=$work/stub-$(basename "$dst")-$$
    "$READELF" --dyn-syms --wide "$src" | awk -v asm="$base.s" -v vs="$base.ver" '
        $1 ~ /^[0-9]+:$/ && $7 != "UND" && NF >= 8 {
            name = $8; ver = ""
            if (index(name, "@@") > 0) { split(name, p, "@@"); name = p[1]; ver = p[2] }
            else if (index(name, "@") > 0) next          # a non-default (compat) version
            if (ver == "GLIBC_PRIVATE" || name == "" || name == ver) next   # name == ver: the version-node symbols
            if (seen[name]++) next
            type = $4; size = $3 + 0; bind = $5
            if (type == "FUNC" || type == "IFUNC") { sec = "text"; t = "@function" }
            else if (type == "OBJECT") { sec = "data"; t = "@object" }
            else if (type == "TLS") { sec = "tbss"; t = "@tls_object" }
            else if (type == "NOTYPE") { sec = "data"; t = "@notype" }
            else next
            if (sec == "text") text[++nt] = name; else if (sec == "data") data[++nd] = name; else tls[++nl] = name
            ty[name] = t; sz[name] = size; bd[name] = bind
            if (ver != "") { if (!(ver in vers)) { order[++nv] = ver; vers[ver] = "" } vers[ver] = vers[ver] "    " name ";\n" }
            else unver = unver "    " name ";\n"
        }
        function emit(n, s) {
            printf "%s %s\n.type %s, %s\n", (bd[n] == "WEAK" ? ".weak" : ".globl"), n, n, ty[n] > asm
            if (ty[n] != "@function") printf ".balign %d\n", (sz[n] >= 16 ? 16 : 8) > asm   # copy relocations take the alignment from here
            printf "%s:\n", n > asm
            if (s > 0) printf ".zero %d\n", s > asm
            printf ".size %s, %d\n", n, sz[n] > asm
        }
        END {
            print ".text" > asm
            for (i = 1; i <= nt; i++) emit(text[i], 1)
            print ".data" > asm
            for (i = 1; i <= nd; i++) emit(data[i], sz[data[i]])
            print ".section .tbss,\"awT\",@nobits" > asm
            for (i = 1; i <= nl; i++) emit(tls[i], sz[tls[i]])
            for (i = 1; i <= nv; i++) printf "%s {\n  global:\n%s};\n", order[i], vers[order[i]] > vs
            if (nv == 0) printf "{\n  global:\n%s  local: *;\n};\n", unver > vs
        }'
    "$CLANG" -target "$tgt" -c "$base.s" -o "$base.o"
    "$LLD" -shared -soname "$soname" --version-script "$base.ver" -o "$dst" "$base.o" --no-undefined-version
    rm -f "$base.s" "$base.o" "$base.ver"
}

# --- compiler-rt builtins ------------------------------------------------------
# builtins TRIPLE CLANG_TARGET OUTDIR CFLAGS...: libclang_rt.builtins.a (and,
# for ELF, crtbegin/crtend objects) from compiler-rt's sources, the files
# CMake's GENERIC/TF/arch lists would pick for the target.
builtins() {
    bt=$1; ct=$2; bo=$3; shift 3
    src=$work/compiler-rt-$COMPILER_RT_VERSION.src/lib/builtins
    if [ ! -d "$src" ]; then
        tar xJf "$(fetch compiler-rt-src)" -C "$work"
    fi
    ob=$work/builtins-$bt
    mkdir -p "$ob"
    skip=" apple_versioning.c os_version_check.c trampoline_setup.c emutls.c enable_execute_stack.c eprintf.c gcc_personality_v0.c crtbegin.c crtend.c clear_cache.c "
    case "$bt" in
        aarch64-*) skip="$skip fp_mode.c " ;;
    esac
    case "$bt" in
        *apple*) skip="$skip atomic.c " ;;   # needs libkern/OSAtomic.h; generic atomics come from libSystem there
    esac
    files=""
    for f in "$src"/*.c; do
        case "$skip" in *" $(basename "$f") "*) continue ;; esac
        case "$bt-$(basename "$f")" in x86_64-*) ;; *xf*.c|*xc3.c) continue ;; esac   # x87 80-bit long double: x86 only
        files="$files $f"
    done
    case "$bt" in
        x86_64-*)
            files="$files $src/x86_64/floatdidf.c $src/x86_64/floatdisf.c $src/x86_64/floatdixf.c $src/x86_64/floatundidf.S $src/x86_64/floatundisf.S $src/x86_64/floatundixf.S $src/cpu_model/x86.c"
            case "$bt" in *windows*) files="$files $src/x86_64/chkstk.S" ;; esac ;;
        aarch64-*apple*)
            files="$files $src/aarch64/fp_mode.c" ;;   # LSE is baseline on Apple CPUs: no outline atomics
        aarch64-*)
            files="$files $src/aarch64/fp_mode.c $src/cpu_model/aarch64.c $src/aarch64/chkstk.S" ;;
    esac
    flags="-target $ct -O2 -fno-builtin -fomit-frame-pointer -fvisibility=hidden -DVISIBILITY_HIDDEN -std=c11 -w -I$src"
    case "$bt" in *windows*) ;; *) flags="$flags -fPIC" ;; esac
    objs=""
    for f in $files; do
        obj="$ob/$(basename "$(dirname "$f")")_$(basename "$f").o"
        "$CLANG" $flags "$@" -c "$f" -o "$obj"
        objs="$objs $obj"
    done
    case "$bt" in
        aarch64-*apple*) ;;
        aarch64-*)
            # outline atomics (-moutline-atomics, clang's default for aarch64
            # Linux): lse.S once per operation, size and memory model, like
            # compiler-rt's CMake does.
            for pat in cas swp ldadd ldclr ldeor ldset; do
                for size in 1 2 4 8 16; do
                    [ "$size" = 16 ] && [ "$pat" != cas ] && continue
                    for model in 1 2 3 4 5; do
                        obj="$ob/lse_${pat}${size}_$model.o"
                        "$CLANG" $flags "$@" -DL_$pat -DSIZE=$size -DMODEL=$model -c "$src/aarch64/lse.S" -o "$obj"
                        objs="$objs $obj"
                    done
                done
            done ;;
    esac
    rm -f "$bo/libclang_rt.builtins.a"
    "$AR" rcs "$bo/libclang_rt.builtins.a" $objs
    case "$bt" in
        *linux*)
            "$CLANG" $flags "$@" -c "$src/crtbegin.c" -o "$bo/crtbegin.o"
            "$CLANG" $flags "$@" -c "$src/crtend.c" -o "$bo/crtend.o"
            "$CLANG" $flags "$@" -DCRT_SHARED -c "$src/crtbegin.c" -o "$bo/crtbeginS.o"
            "$CLANG" $flags "$@" -DCRT_SHARED -c "$src/crtend.c" -o "$bo/crtendS.o" ;;
    esac
}

# runtime CROSS TRIPLE OUTDIR MAKEVARS...: libzell_rt.a for the target, built
# by runtime/Makefile into runtime/build/bundle/<triple>/ (not the developer's
# runtime/build/<triple>/), without debug information.
runtime() {
    rc=$1; rtr=$2; ro=$3; shift 3
    b=build/bundle/$rtr
    rm -rf "$root/runtime/$b"
    # not -s: a failing compile must show its command line in the CI log
    make -C "$root/runtime" -j"$JOBS" CROSS="$rc" BUILD="$b" CLANG="$CLANG" "CFLAGS_EXTRA=-g0" "$@" "$b/libzell_rt.a" | grep -vE '^(clang|cc|ar) ' || true
    cp "$root/runtime/$b/libzell_rt.a" "$ro/libzell_rt.a"
    "$STRIP" --strip-debug "$ro/libzell_rt.a"
}

bundle_file() { # DIR key=value...
    d=$1; shift
    { echo "# Zell toolchain link inputs (scripts/bundle-targets.sh); see docs/PLATFORMS.md, \"Дистрибутив\""; for kv in "$@"; do echo "$kv"; done; } > "$d/BUNDLE"
}

# --- linux-gnu -----------------------------------------------------------------
linux_gnu() {
    t=$1
    cpu=${t%%-*}
    case "$cpu" in
        x86_64) deb=amd64; ld=ld-linux-x86-64.so.2; interp=/lib64/$ld ;;
        aarch64) deb=arm64; ld=ld-linux-aarch64.so.1; interp=/lib/$ld ;;
    esac
    ma=$cpu-linux-gnu
    sr=$work/sysroot-$t
    for p in libc6-$deb libc6-dev-$deb linux-libc-dev-$deb libsqlite3-0-$deb libsqlite3-dev; do undeb "$p" "$sr"; done
    # the -dev packages link libfoo.so to absolute /lib/... paths; point them into the sysroot
    o=$out/$t
    rm -rf "$o"; mkdir -p "$o"
    for f in Scrt1.o crt1.o crti.o crtn.o libc_nonshared.a; do cp "$sr/usr/lib/$ma/$f" "$o/"; done
    L=$sr/lib/$ma
    elf_stub "$L/libc-$GLIBC_VERSION.so" "$o/libc.so" libc.so.6 "$ma"
    elf_stub "$L/libm-$GLIBC_VERSION.so" "$o/libm.so" libm.so.6 "$ma"
    elf_stub "$L/libpthread-$GLIBC_VERSION.so" "$o/libpthread.so" libpthread.so.0 "$ma"
    elf_stub "$L/libdl-$GLIBC_VERSION.so" "$o/libdl.so" libdl.so.2 "$ma"
    elf_stub "$L/librt-$GLIBC_VERSION.so" "$o/librt.so" librt.so.1 "$ma"
    elf_stub "$L/libutil-$GLIBC_VERSION.so" "$o/libutil.so" libutil.so.1 "$ma"
    elf_stub "$L/ld-$GLIBC_VERSION.so" "$o/$ld" "$ld" "$ma"
    sq=$(ls "$sr/usr/lib/$ma"/libsqlite3.so.0.* | head -1)
    elf_stub "$sq" "$o/libsqlite3.so" libsqlite3.so.0 "$ma"
    # headers for the runtime build: glibc 2.31 + kernel 5.4 + sqlite3.h; the
    # libsqlite3.so symlink must resolve inside the sysroot
    builtins "$t" "$ma" "$o" --sysroot="$sr" -nostdinc -isystem "$("$CLANG" -print-resource-dir)/include" -isystem "$sr/usr/include/$ma" -isystem "$sr/usr/include"
    runtime "$ma" "$t" "$o" SYSROOT="$sr"
    bundle_file "$o" "triple=$t" "libc=glibc $GLIBC_VERSION (stubs; executables need glibc >= $GLIBC_VERSION)" \
        "dynamic-linker=$interp" "sqlite=$SQLITE_VERSION (stub, libsqlite3.so.0 at run time)" \
        "compiler-rt=$COMPILER_RT_VERSION"
}

# --- linux-musl ----------------------------------------------------------------
linux_musl() {
    t=$1
    cpu=${t%%-*}
    mt=$cpu-linux-musl
    src=$work/musl-$MUSL_VERSION
    [ -d "$src" ] || tar xzf "$(fetch musl-src)" -C "$work"
    mb=$work/musl-build-$cpu
    mr=$work/musl-root-$cpu
    mkdir -p "$mb"
    (cd "$mb" && CC="$CLANG -target $mt" AR="$AR" RANLIB="$RANLIB" CFLAGS="-O2 -g0" \
        "$src/configure" --target="$mt" --prefix=/ --disable-shared >/dev/null &&
        make -s -j"$JOBS" AR="$AR" RANLIB="$RANLIB" >/dev/null && make -s install DESTDIR="$mr" >/dev/null)
    o=$out/$t
    rm -rf "$o"; mkdir -p "$o"
    for f in crt1.o rcrt1.o Scrt1.o crti.o crtn.o libc.a; do cp "$mr/lib/$f" "$o/"; done
    builtins "$t" "$mt" "$o" -nostdinc -isystem "$mr/include" -isystem "$("$CLANG" -print-resource-dir)/include"
    runtime "$mt" "$t" "$o" MUSL_INC="$mr/include"
    bundle_file "$o" "triple=$t" "libc=musl $MUSL_VERSION (static)" "compiler-rt=$COMPILER_RT_VERSION"
}

# --- windows (MinGW ABI) ---------------------------------------------------------
WINDOWS_LIBS="kernel32 user32 advapi32 shell32 ws2_32 iphlpapi bcrypt ntdll crypt32 dbghelp ole32 oleaut32 uuid userenv secur32 gdi32 winmm version psapi mswsock shlwapi ncrypt"
windows() {
    t=$1
    mr=$work/mingw
    undeb mingw-w64-x86-64-dev "$mr"
    undeb mingw-w64-common "$mr"
    o=$out/$t
    rm -rf "$o"; mkdir -p "$o"
    L=$mr/usr/x86_64-w64-mingw32/lib
    for f in crt2.o dllcrt2.o libmingw32.a libmingwex.a libmoldname.a libmsvcrt.a; do cp "$L/$f" "$o/"; done
    for l in $WINDOWS_LIBS; do cp "$L/lib$l.a" "$o/"; done
    builtins "$t" x86_64-w64-mingw32 "$o" --sysroot="$mr/usr"
    runtime x86_64-w64-mingw32 "$t" "$o" SYSROOT="$mr/usr"
    bundle_file "$o" "triple=$t" "libc=mingw-w64 $MINGW_VERSION (msvcrt.dll)" "compiler-rt=$COMPILER_RT_VERSION"
}

# --- macOS -------------------------------------------------------------------
# libSystem.tbd: a TAPI text stub of /usr/lib/libSystem.B.dylib (like the
# ones Zig ships) listing what the runtime archive, the builtins and the
# generated code import, plus the common C and POSIX API (scripts/lld/
# darwin-symbols.txt, all present in libSystem since macOS 11). Symbols live
# in re-exported sub-libraries (libsystem_c, libsystem_kernel, ...); dyld
# resolves a two-level lookup through libSystem's re-exports, so one flat
# list links and runs.
darwin() {
    t=$1
    cpu=${t%%-*}
    arch=$cpu; [ "$cpu" = aarch64 ] && arch=arm64
    o=$out/$t
    rm -rf "$o"; mkdir -p "$o"
    sdk=${SDKROOT:-}
    if [ -z "$sdk" ] && command -v xcrun >/dev/null 2>&1; then sdk=$(xcrun --sdk macosx --show-sdk-path 2>/dev/null || true); fi
    if [ -n "$sdk" ]; then
        # on a Mac (release builds): the real SDK's headers
        builtins "$t" "$arch-apple-macos$MACOS_MIN" "$o" -isysroot "$sdk"
        runtime "$t" "$t" "$o" SDKROOT="$sdk" STUBSDK= MACOS_MIN="$MACOS_MIN"
        built="the macOS SDK $(basename "$sdk")"
    else
        builtins "$t" "$arch-apple-macos$MACOS_MIN" "$o" -nostdlibinc -isystem "$root/runtime/stub-sdk/macos/include"
        runtime "$t" "$t" "$o" STUBSDK=yes MACOS_MIN="$MACOS_MIN"
        built="runtime/stub-sdk/macos"
    fi
    syms=$work/darwin-$arch.syms
    {
        "$NM" -u "$o/libzell_rt.a" "$o/libclang_rt.builtins.a" 2>/dev/null | awk 'NF == 2 && $1 == "U" { print $2 } NF == 1 && $1 !~ /:$/ { print $1 }'
        sed -e 's/#.*//' -e '/^[[:space:]]*$/d' -e 's/^\([^=]\)/_\1/' -e 's/^=//' "$root/scripts/lld/darwin-symbols.txt"
    } | sort -u > "$syms.all"
    "$NM" --defined-only "$o/libzell_rt.a" "$o/libclang_rt.builtins.a" 2>/dev/null | awk 'NF == 3 { print $3 }' | sort -u > "$syms.def"
    comm -23 "$syms.all" "$syms.def" > "$syms.need"
    grep -v '^_sqlite3_' "$syms.need" > "$syms" || true
    grep '^_sqlite3_' "$syms.need" > "$syms.sqlite" || true
    tbd() { # file install-name symbols
        {
            echo "--- !tapi-tbd"
            echo "tbd-version: 4"
            echo "targets: [ $arch-macos ]"
            echo "install-name: '$2'"
            echo "exports:"
            echo "  - targets: [ $arch-macos ]"
            echo "    symbols: ["
            sed -e 's/^/      '"'"'/' -e 's/$/'"'"',/' "$3" | sed '$ s/,$//'
            echo "    ]"
            echo "..."
        } > "$1"
    }
    # /usr/lib/libsqlite3.dylib: what the runtime's database.sqlite imports
    [ -s "$syms.sqlite" ] && tbd "$o/libsqlite3.tbd" /usr/lib/libsqlite3.dylib "$syms.sqlite"
    {
        echo "--- !tapi-tbd"
        echo "tbd-version: 4"
        echo "targets: [ $arch-macos ]"
        echo "install-name: '/usr/lib/libSystem.B.dylib'"
        echo "current-version: 1319"
        echo "exports:"
        echo "  - targets: [ $arch-macos ]"
        echo "    symbols: ["
        sed -e 's/^/      '"'"'/' -e 's/$/'"'"',/' "$syms" | sed '$ s/,$//'
        echo "    ]"
        echo "..."
    } > "$o/libSystem.tbd"
    bundle_file "$o" "triple=$t" "libc=libSystem (text stub, $(wc -l < "$syms" | tr -d ' ') symbols; macOS >= $MACOS_MIN)" \
        "runtime=built against $built" "sqlite=$( [ -s "$syms.sqlite" ] && echo "libsqlite3.tbd ($(wc -l < "$syms.sqlite" | tr -d ' ') symbols)" || echo none)" "compiler-rt=$COMPILER_RT_VERSION"
}

for t in $targets; do
    echo "== $t" >&2
    case "$t" in
        *-linux-gnu) linux_gnu "$t" ;;
        *-linux-musl) linux_musl "$t" ;;
        *-windows-gnu) windows "$t" ;;
        *-apple-darwin) darwin "$t" ;;
        *) echo "bundle-targets.sh: unknown target $t" >&2; exit 2 ;;
    esac
    printf '%s  %s\n' "$(du -sk "$out/$t" | cut -f1) KB" "$out/$t" >&2
done
# provenance of every download, next to the targets (package-toolchain.sh
# copies it into THIRD_PARTY.md's inputs list)
printf '%s\n' "$PINS" | sed '/^$/d' | awk -F'|' '{ printf "%s  %s\n", $3, $2 }' > "$out/SOURCES.sha256"
rm -rf "$work"
echo "targets in $out" >&2
