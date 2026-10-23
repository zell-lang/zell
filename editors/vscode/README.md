# Zell for VS Code

Language support for `.zl` files:

- **Syntax highlighting** without a server: the TextMate grammar in
  `syntaxes/zell.tmLanguage.json`.
- **Language server** (`zell-lsp`, the `lsp/` crate of this repository):
  diagnostics as you type (also in half-written code: the parser recovers),
  quick fixes (unused variable, unhandled `T!`, unused or missing import),
  document outline, go to definition, find references, highlights, hover
  (signature + doc comment), rename across files, semantic highlighting,
  folding, completion (members after `.`, names in scope, keywords),
  signature help, inlay hints (inferred `var` types, parameter names),
  formatting (`zell fmt` rules, whole document or selection), workspace
  symbols, go to implementation / type definition, smart selection and
  code lenses: "▶ Run test" on `@test` functions (`zell test --filter
  ^Name$`), "▶ Run" on `main` (`zell run`, both as tasks) and reference
  counts on exported declarations. Renaming or moving a `.zl` file in a
  package updates its `namespace` line and the imports when the namespace
  follows the directory (`src/a/b/x.zl` -> `app.a.b`).

## Install

### From the toolchain archive

The toolchain archive (see the repository README, "Install") contains
`bin/zell-lsp` and the extension sources in `tools/vscode`. With the
toolchain on `PATH` (the installer does this) the extension finds the
server by itself. Build and install the extension from those sources
(Node.js 18+ and npm are needed):

```sh
cd ~/.zell/toolchain/current/tools/vscode
npm install
npx @vscode/vsce package
code --install-extension zell-*.vsix
```

### From source

Build the server from the repository root (a source checkout needs LLVM
18, see the README):

```sh
cargo build --release -p zell-lsp     # target/release/zell-lsp
```

Put `target/release` on `PATH`, or point the `zell.serverPath` setting at
the binary. If `zell-lsp` is not on `PATH` but the `zell` CLI is, the
extension runs `zell lsp` instead (which starts the `zell-lsp` binary
installed next to `zell`). Then build and package the extension:

```sh
cd editors/vscode
npm install
npm run build                  # type-checks with tsc and bundles to out/extension.js
npx @vscode/vsce package       # zell-*.vsix
code --install-extension zell-*.vsix     # or Extensions view, "Install from VSIX..."
```

The extension is not on the Marketplace yet.

### Run it in development

Open `editors/vscode` in VS Code and press **F5** ("Run Zell
extension"): an Extension Development Host opens on the repository's
`examples/` folder. Open any `.zl` file to start the server; its log is in
the **Output** panel under "Zell" (set `"zell.trace.server": "verbose"` to
see the LSP traffic). `npm run watch` rebuilds the bundle on every change.

## Settings and commands

| Setting | Default | Meaning |
|---------|---------|---------|
| `zell.serverPath` | `zell-lsp` | Server executable (a path to `zell` runs `zell lsp`) |
| `zell.serverArgs` | `[]` | Extra server arguments |
| `zell.trace.server` | `off` | `messages` / `verbose` LSP tracing |
| `zell.inlayHints.types.enabled` | `true` | inferred type after `var x = ...` / `for x in ...` names |
| `zell.inlayHints.parameterNames.enabled` | `true` | parameter names before literal call arguments |
| `zell.path` | `zell` | CLI used to build for debugging and to find the formatters |
| `zell.debug.lldbDapPath` | `""` | `lldb-dap` executable (default: `lldb-dap`, `lldb-dap-18`, ... on `PATH`) |
| `zell.debug.formatters` | `""` | path of `zell_formatters.py` (default: `zell debug --formatters lldb`) |

Inlay hints are the two `zell.inlayHints.*` settings (the server also reads
them as `initializationOptions`). Turn either off in the settings UI or in
`settings.json`:

```json
"zell.inlayHints.types.enabled": false,
"zell.inlayHints.parameterNames.enabled": false
```

Command palette: **Zell: Restart server** (also done automatically when
`zell.serverPath` changes) and **Zell: Format document (zell fmt)**. The
code lenses above `main` and `@test` functions run **Zell: Run**, **Run
test** and **Debug test** (`zell.run`, `zell.runTest`, `zell.debugTest`);
`zell.showReferences` backs the reference-count lenses.

### Formatting

The server formats with the canonical `zell fmt` style (the same code as
the CLI). The extension makes itself the default formatter for `.zl` files
and turns on format-on-save for them (`configurationDefaults`); to opt out,
add to your settings:

```json
"[zell]": { "editor.formatOnSave": false }
```

**Format Selection** formats only the changed lines inside the selection.
A file with syntax errors is left unchanged.

## Debugging

The extension contributes the debug type **`zell`** with two adapters,
chosen by `"adapter"`:

- **`"agent"`** (default): the in-process debug agent that every debug
  build links (`debug`, docs/DEBUGGER.md stage 2). `launch` starts the
  program with `ZELL_DEBUG=127.0.0.1:0,wait`, reads the port it announces
  on stderr (`zell-debug: listening on 127.0.0.1:PORT`) and connects VS
  Code to it (`DebugAdapterServer`); the program's output goes to the debug
  console and the program is killed with the session. Breakpoints with
  conditions, hit counts and logpoints (`{expr}` interpolation, no stop),
  function breakpoints, exception breakpoints (*Panics*; *Errors (on
  creation)* with an optional type filter such as `NotFound`), stepping
  that follows a task across `await`, pause, one entry per task in the
  call-stack view, variables formatted as Zell values with paging of large
  slices and maps, and watch / hover / debug-console expressions (variable
  paths `a.b[2]`, `m["k"]`, comparisons) work. No lldb is needed.
- **`"lldb"`**: [`lldb-dap`](https://lldb.llvm.org/use/lldbdap.html) (LLDB
  18+; install `lldb-18` on Ubuntu, or point `zell.debug.lldbDapPath` at
  it) with the Zell formatters (`tools/lldb/zell_formatters.py`, located
  with `zell debug --formatters lldb`) loaded through `initCommands`;
  expressions in the debug console are lldb's (C syntax). Use it for
  attaching by pid to a program without the agent.

F5 on a `.zl` file without a `launch.json` builds it (`zell build`; debug
info and the agent are on by default) and debugs it. A `launch.json`:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "zell",
      "request": "launch",
      "name": "Debug Zell file",
      "source": "${file}",
      "program": "${workspaceFolder}/target/zell-debug/${fileBasenameNoExtension}",
      "args": [],
      "env": { "ZELL_WORKERS": "2" },
      "cwd": "${workspaceFolder}",
      "preLaunchTask": "zell: build"
    },
    { "type": "zell", "request": "attach", "name": "Attach to agent", "address": ":2345" },
    { "type": "zell", "request": "attach", "name": "Attach (lldb)", "adapter": "lldb", "pid": "${command:pickProcess}" }
  ]
}
```

- `launch`: `program` (executable), or `source` (file / project) which is
  built first unless a `preLaunchTask` is set (`build: false` skips it,
  `buildArgs` adds flags; with `"--release"` the extension adds
  `--debug-agent`); `args`, `env` (object or `["K=V"]`), `cwd`,
  `stopOnEntry`; with `"adapter": "lldb"` also lldb-dap's `initCommands`,
  `preRunCommands`, `stopCommands`.
- `attach`: agent — `address` (`"host:port"` or `":port"`) or `host` +
  `port` of a program started with `ZELL_DEBUG=:2345` (or
  `zell debug run app.zl --headless --port 2345`); lldb — `pid` (asked
  for when missing), optionally `program`.
- Task type `zell` (`"command": "build"`, `source`, `program`, `args`):
  the provided task **zell: build** runs
  `zell build ${file} -o target/zell-debug/<name>`.
- With lldb, `zell-next` in the debug console steps over a statement and
  follows an `await` to the coroutine's next line (plain `next` stops in
  the caller when the `await` suspends); a panic stops at `zell_panic`.

## Project discovery

On start the server loads every `.zl` file of each workspace folder. If the
folder has a manifest (`zell.toml`, `zell.json` or `zell.yaml`) and a
`src/` directory, only `src/**` is loaded. A file opened outside the
workspace loads the project whose manifest is found walking up from it.
