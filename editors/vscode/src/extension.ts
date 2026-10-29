// VS Code client for the Zell language server (zell-lsp).
//
// Launches the server over stdio, found in this order: `zell.serverPath`,
// `zell-lsp` on PATH, `zell-lsp` next to the `zell` CLI on PATH (the
// toolchain archive layout `bin/zell-lsp`), `zell lsp`; with none of them
// an error explains what to install or set. A status bar item shows the
// analyzer state (`zell/analyzerStatus`). The `zell.*`
// settings reach the server as initialization options, pulled with
// workspace/configuration and on workspace/didChangeConfiguration; it reads
// `zell.inlayHints.*`. `}` and Enter format the enclosing block
// (textDocument/onTypeFormatting, `editor.formatOnType` on by default).
//
// Code lens commands sent by the server are executed here:
// `zell.runTest` / `zell.debugTest` (file URI, test name: `zell test
// <package or file> --filter ^Name$ -v` as a task), `zell.run` (file URI:
// `zell run`) and `zell.showReferences` (URI, position, locations: the
// references peek view).

import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { Executable, LanguageClient, LanguageClientOptions, ServerOptions, State } from "vscode-languageclient/node";
import { registerDebugging } from "./debug";

let client: LanguageClient | undefined;
let output: vscode.OutputChannel | undefined;
let status: vscode.StatusBarItem | undefined;
let statusTimer: NodeJS.Timeout | undefined;

/** Finds `name` on PATH (with Windows extensions); undefined if missing. */
function which(name: string): string | undefined {
  if (name.includes("/") || name.includes("\\")) {
    return fs.existsSync(name) ? name : undefined;
  }
  const exts = process.platform === "win32" ? (process.env.PATHEXT ?? ".EXE;.CMD;.BAT").split(";") : [""];
  for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
    for (const ext of exts) {
      const candidate = path.join(dir, name + ext);
      try {
        if (fs.statSync(candidate).isFile()) {
          return candidate;
        }
      } catch {
        // not here
      }
    }
  }
  return undefined;
}

/** Resolves the server command from the settings and PATH; undefined when
 * no server can be found. */
function serverExecutable(): Executable | undefined {
  const config = vscode.workspace.getConfiguration("zell");
  const configured = config.get<string>("serverPath", "zell-lsp").trim() || "zell-lsp";
  const extraArgs = config.get<string[]>("serverArgs", []);
  const base = path.basename(configured).replace(/\.exe$/i, "");
  // A path to the `zell` CLI: run its `lsp` subcommand.
  if (base === "zell") {
    return { command: configured, args: ["lsp", ...extraArgs] };
  }
  if (configured !== "zell-lsp") {
    return { command: configured, args: extraArgs };
  }
  const onPath = which("zell-lsp");
  if (onPath) {
    return { command: onPath, args: extraArgs };
  }
  const cli = which("zell");
  if (cli) {
    let dir = path.dirname(cli);
    try {
      dir = path.dirname(fs.realpathSync(cli));
    } catch {
      // keep the PATH entry
    }
    const sibling = path.join(dir, process.platform === "win32" ? "zell-lsp.exe" : "zell-lsp");
    if (fs.existsSync(sibling)) {
      output?.appendLine(`zell-lsp not on PATH; using ${sibling} next to ${cli}`);
      return { command: sibling, args: extraArgs };
    }
    output?.appendLine("zell-lsp not found on PATH or next to zell; using `zell lsp`");
    return { command: cli, args: ["lsp", ...extraArgs] };
  }
  return undefined;
}

/** Polls `zell/analyzerStatus` into the status bar item. */
async function refreshStatus(): Promise<void> {
  if (!status) {
    return;
  }
  if (!vscode.workspace.getConfiguration("zell").get<boolean>("statusBar.enabled", true)) {
    status.hide();
    return;
  }
  status.show();
  if (!client || client.state !== State.Running) {
    status.text = client?.state === State.Starting ? "$(sync~spin) Zell" : "$(error) Zell";
    status.tooltip = client?.state === State.Starting ? "Zell: starting the language server" : "Zell: the language server is not running (click for its output)";
    return;
  }
  try {
    const s = await client.sendRequest<{ version: number; analysisVersion: number; inFlight: boolean; queued: number; stats: Record<string, number> }>("zell/analyzerStatus", null);
    const busy = s.inFlight || s.queued > 0 || s.version !== s.analysisVersion;
    status.text = busy ? "$(sync~spin) Zell" : "$(check) Zell";
    status.tooltip = `Zell: ${busy ? "analysing" : "ready"} (version ${s.version}, analysed ${s.analysisVersion}, ${s.queued} queued)\n` + Object.entries(s.stats).map(([k, v]) => `${k}: ${v}`).join(", ");
  } catch {
    // the server is restarting
  }
}

/** The settings the server reads (`zell.inlayHints.*`). */
function serverSettings(): object {
  const config = vscode.workspace.getConfiguration("zell");
  return {
    inlayHints: {
      types: { enabled: config.get<boolean>("inlayHints.types.enabled", true) },
      parameterNames: { enabled: config.get<boolean>("inlayHints.parameterNames.enabled", true) },
    },
  };
}

function createClient(exe: Executable): LanguageClient {
  output?.appendLine(`starting ${exe.command} ${(exe.args ?? []).join(" ")}`);
  const serverOptions: ServerOptions = { run: exe, debug: exe };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "zell" },
      { scheme: "untitled", language: "zell" },
    ],
    outputChannel: output,
    // The server also pulls the `zell` section (workspace/configuration)
    // and registers its own file watchers (`**/*.zl`, `zell.toml`,
    // `zell.lock`) through client/registerCapability.
    initializationOptions: serverSettings(),
  };
  return new LanguageClient("zell", "Zell Language Server", serverOptions, clientOptions);
}

async function startClient(): Promise<void> {
  const exe = serverExecutable();
  if (!exe) {
    const choice = await vscode.window.showErrorMessage(
      "Zell: the language server was not found: zell-lsp is not on PATH, and neither is the zell CLI. Install the Zell toolchain (bin/zell and bin/zell-lsp) or set \"zell.serverPath\".",
      "Open Settings",
    );
    if (choice === "Open Settings") {
      void vscode.commands.executeCommand("workbench.action.openSettings", "zell.serverPath");
    }
    return;
  }
  client = createClient(exe);
  try {
    await client.start();
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    void vscode.window.showErrorMessage(
      `Zell: could not start the language server (${msg}). Install zell-lsp or set "zell.serverPath".`,
    );
  }
}

async function restartClient(): Promise<void> {
  if (client) {
    const old = client;
    client = undefined;
    try {
      await old.stop();
    } catch {
      // already stopped
    }
  }
  await startClient();
}

/** The `zell` CLI (setting `zell.path`). */
function zellCli(): string {
  return vscode.workspace.getConfiguration("zell").get<string>("path", "zell").trim() || "zell";
}

/** The package directory of `file` (nearest manifest upwards), if any. */
function packageDir(file: string): string | undefined {
  let dir = path.dirname(file);
  for (;;) {
    if (["zell.toml", "zell.json", "zell.yaml", "zell.yml"].some((m) => fs.existsSync(path.join(dir, m)))) {
      return dir;
    }
    const parent = path.dirname(dir);
    if (parent === dir) {
      return undefined;
    }
    dir = parent;
  }
}

/** Runs `zell <args>` as a task named `name` in the folder of `file`. */
async function runZellTask(file: string, name: string, args: string[]): Promise<void> {
  const folder = vscode.workspace.getWorkspaceFolder(vscode.Uri.file(file));
  const cwd = folder?.uri.fsPath ?? path.dirname(file);
  const task = new vscode.Task(
    { type: "zell", command: args[0] },
    folder ?? vscode.TaskScope.Workspace,
    name,
    "zell",
    new vscode.ProcessExecution(zellCli(), args, { cwd }),
  );
  task.presentationOptions = { reveal: vscode.TaskRevealKind.Always, clear: true };
  await vscode.tasks.executeTask(task);
}

/** Code lens `zell.runTest`: runs one `@test` function. */
async function runTest(uri: string, name: string): Promise<void> {
  const file = vscode.Uri.parse(uri).fsPath;
  await runZellTask(file, `test ${name}`, ["test", packageDir(file) ?? file, "--filter", `^${name}$`, "-v"]);
}

/** Code lens `zell.debugTest`: `zell test` has no debug mode yet, so the test runs verbosely. */
async function debugTest(uri: string, name: string): Promise<void> {
  void vscode.window.showInformationMessage(
    `Zell: debugging a single test is not supported by \`zell test\` yet; running ${name} with -v instead.`,
  );
  await runTest(uri, name);
}

/** Code lens `zell.run`: `zell run` of the package (or the file). */
async function runMain(uri: string): Promise<void> {
  const file = vscode.Uri.parse(uri).fsPath;
  await runZellTask(file, "run", ["run", packageDir(file) ?? file]);
}

interface LspPosition {
  line: number;
  character: number;
}

interface LspLocation {
  uri: string;
  range: { start: LspPosition; end: LspPosition };
}

/** Code lens `zell.showReferences`: the references peek view. */
async function showReferences(uri: string, position: LspPosition, locations: LspLocation[]): Promise<void> {
  const toPos = (p: LspPosition) => new vscode.Position(p.line, p.character);
  const locs = locations.map((l) => new vscode.Location(vscode.Uri.parse(l.uri), new vscode.Range(toPos(l.range.start), toPos(l.range.end))));
  await vscode.commands.executeCommand("editor.action.showReferences", vscode.Uri.parse(uri), toPos(position), locs);
}

/** `Zell: Format document`: formats the active .zl file through the server (`zell fmt` rules). */
async function formatDocument(): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.languageId !== "zell") {
    void vscode.window.showInformationMessage("Zell: open a .zl file to format it.");
    return;
  }
  await vscode.commands.executeCommand("editor.action.formatDocument");
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  output = vscode.window.createOutputChannel("Zell");
  context.subscriptions.push(output);
  context.subscriptions.push(vscode.commands.registerCommand("zell.restartServer", restartClient));
  context.subscriptions.push(vscode.commands.registerCommand("zell.showOutput", () => output?.show(true)));
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 0);
  status.name = "Zell";
  status.command = "zell.showOutput";
  context.subscriptions.push(status);
  statusTimer = setInterval(() => void refreshStatus(), 1500);
  context.subscriptions.push({ dispose: () => statusTimer && clearInterval(statusTimer) });
  context.subscriptions.push(vscode.commands.registerCommand("zell.fmt", formatDocument));
  context.subscriptions.push(vscode.commands.registerCommand("zell.runTest", runTest));
  context.subscriptions.push(vscode.commands.registerCommand("zell.debugTest", debugTest));
  context.subscriptions.push(vscode.commands.registerCommand("zell.run", runMain));
  context.subscriptions.push(vscode.commands.registerCommand("zell.showReferences", showReferences));
  registerDebugging(context);
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("zell.serverPath") || e.affectsConfiguration("zell.serverArgs")) {
        void restartClient();
      } else if (e.affectsConfiguration("zell.statusBar")) {
        void refreshStatus();
      } else if (e.affectsConfiguration("zell.inlayHints") && client) {
        void client.sendNotification("workspace/didChangeConfiguration", { settings: { zell: serverSettings() } });
      }
    }),
  );
  await startClient();
  void refreshStatus();
}

export async function deactivate(): Promise<void> {
  if (client) {
    await client.stop();
    client = undefined;
  }
}
