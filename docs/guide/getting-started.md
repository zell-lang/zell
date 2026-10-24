# Getting started

From nothing to a tested, cross-compiled program and a configured editor.

## 1. Install

The prebuilt toolchain is one self-contained archive per host (no clang, no
C headers, no SDK needed). On Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/zell-lang/zell/master/scripts/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/zell-lang/zell/master/scripts/install.ps1 | iex
```

A release candidate is a GitHub pre-release and has to be requested by
version: `... | sh -s -- --version 0.1.0-rc.1`. Archives, checksums,
unpacking by hand and the macOS quarantine note are in the
[README](../../README.md#install); building from source is described there
too. Check the result:

```sh
zell version
zell doctor      # the linker and the link inputs of every target
```

## 2. Create and run a project

```sh
zell init hello && cd hello
zell run
```

`zell init` writes `zell.toml` and `src/main.zl`:

```zell
namespace hello

import io

func main() {
    println("Hello, world!")
}
// Output:
// Hello, world!
```

`zell init` is optional. A single file runs without a project
(`zell run hello.zl`), and so does a plain directory of `.zl` files
(`zell run .`). The smallest project is a directory with `main.zl` and a
`zell.toml` beside it:

```toml
[package]
name = "hello"
version = "0.1.0"
entry = "main.zl"
```

The files then declare `namespace hello`: the package name is the root of
its namespaces unless the manifest sets `namespace`. `zell run` finds the
manifest from the current directory. Every command has its own help:
`zell --help` lists the commands and `zell run --help` shows the build
options. More about manifests and dependencies: [Packages](packages.md).

## 3. Test and format

Put a test next to the code, in `src/main_test.zl`; a test is a function
marked `@test`:

```zell
// zell test
namespace hello

func Greeting(name string) string {
    return "Hello, " + name + "!"
}

@test func TestGreeting(t *testing.T) {
    testing.Equal(t, Greeting("Zell"), "Hello, Zell!")
}
```

```sh
zell test            # -v for names and logs, --filter <regex>, --bench
zell fmt             # rewrite files in the canonical style
zell fmt --check     # for CI: fail if anything would change
```

See [Testing](testing.md) for table tests, benchmarks and runnable examples.

## 4. Build and cross-compile

```sh
zell build --release                              # an optimized, stripped binary
zell build --release --target linux-arm64-musl    # a static arm64 Linux executable
zell build --target windows-amd64                 # hello.exe
zell target list                                  # every target and its link inputs
```

With the toolchain archive every target links out of the box, from any
host. Linux (glibc) executables need glibc 2.31 or newer; the `-musl`
targets are fully static. If a target is reported as not installed (a
source checkout, or an extra target), `zell target add <target>` downloads
its runtime.

## 5. Set up the editor

- **VS Code**: build or install the extension from
  [editors/vscode](../../editors/vscode/README.md); the toolchain archive
  ships its sources in `tools/vscode`.
- **IntelliJ IDEA Ultimate, GoLand and other commercial JetBrains IDEs**:
  install the zip from the
  [zell-idea-plugin releases](https://github.com/zell-lang/zell-idea-plugin/releases)
  with *Settings > Plugins > Install Plugin from Disk*. The free Community
  edition has no LSP API and cannot host `zell-lsp`.
- Any other editor with an LSP client: point it at `zell-lsp` (setups for
  Neovim and Helix are in the [README](../../README.md#the-language-server-and-other-editors)).

## Where next

[A tour of Zell](tour.md), then [Errors](errors.md) and [Async](async.md).
Larger programs to read and run: [docs/EXAMPLES.md](../EXAMPLES.md).
