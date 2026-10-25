//! Debugger stage 1 (docs/DEBUGGER.md): DWARF from codegen, the lldb
//! formatters (tools/lldb), the gdb printers (tools/gdb) and lldb-dap.
//!
//! The fixtures in `tests/fixtures/debug/` mark breakpoint lines with
//! `// BREAK <name>`; the tests stop there with `lldb --batch` and compare
//! the formatted variables. Tests whose tool (lldb, gdb, lldb-dap,
//! llvm-dwarfdump, llvm-objdump) is missing are skipped with a message.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn zell() -> &'static str {
    env!("CARGO_BIN_EXE_zell")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/debug").join(name)
}

fn formatters() -> PathBuf {
    repo().join("tools/lldb/zell_formatters.py")
}

fn ensure_runtime() {
    let lib = repo().join("runtime/libzell_rt.a");
    if !lib.is_file() {
        let st = Command::new("make").arg("-C").arg(repo().join("runtime")).status().expect("cannot run make");
        assert!(st.success(), "make -C runtime failed");
    }
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zell-debug-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The first of `names` that runs with `--version`.
fn tool(names: &[&str]) -> Option<String> {
    names.iter().find(|n| Command::new(n).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())).map(|s| s.to_string())
}

fn lldb() -> Option<String> {
    let t = tool(&["lldb", "lldb-18"]);
    if t.is_none() {
        eprintln!("skipped: lldb is not installed");
    }
    t
}

/// `zell build <src> -o <out> <flags>`.
fn build(src: &Path, out: &Path, flags: &[&str]) {
    ensure_runtime();
    let o = Command::new(zell()).arg("build").arg(src).arg("-o").arg(out).args(flags).output().unwrap();
    assert!(o.status.success(), "zell build {} failed:\n{}", src.display(), String::from_utf8_lossy(&o.stderr));
}

/// 1-based line of the `// BREAK <name>` marker in `src`.
fn marker(src: &Path, name: &str) -> usize {
    let text = std::fs::read_to_string(src).unwrap();
    let tag = format!("// BREAK {name}");
    1 + text.lines().position(|l| l.trim_end().ends_with(&tag)).unwrap_or_else(|| panic!("no marker {tag} in {}", src.display()))
}

/// 1-based line of the first line of `src` containing `needle`.
fn line_with(src: &Path, needle: &str) -> usize {
    let text = std::fs::read_to_string(src).unwrap();
    1 + text.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("no line with {needle:?} in {}", src.display()))
}

/// Runs `lldb --batch` on `exe` with the formatters and `cmds`; stdout.
fn run_lldb(lldb: &str, exe: &Path, cmds: &[String]) -> String {
    let mut c = Command::new(lldb);
    c.arg("--batch").arg("--no-lldbinit");
    c.arg("-o").arg(format!("command script import {}", formatters().display()));
    for cmd in cmds {
        c.arg("-o").arg(cmd);
    }
    c.arg("--").arg(exe);
    let o = c.output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    let err = String::from_utf8_lossy(&o.stderr).to_string();
    assert!(!out.contains("Traceback") && !err.contains("Traceback"), "formatter exception:\n{out}\n{err}");
    out
}

fn assert_has(out: &str, want: &str) {
    assert!(out.lines().any(|l| l.trim() == want || l.contains(want)), "expected {want:?} in lldb output:\n{out}");
}

#[test]
fn lldb_formats_every_value_kind() {
    let Some(lldb) = lldb() else { return };
    let src = fixture("values.zl");
    let dir = tmpdir("values");
    let exe = dir.join("values");
    build(&src, &exe, &[]);
    let out = run_lldb(
        &lldb,
        &exe,
        &[
            format!("b values.zl:{}", marker(&src, "values")),
            "run".into(),
            "frame variable".into(),
            "frame variable nums[1] *pp child->parent".into(),
        ],
    );
    for want in [
        r#"(string) name = "zell""#,
        r#"(string) empty = """#,
        "([]int) nums = [5, 3, 8]",
        "([]int) none = []",
        r#"(map[string]int) ages = {"alice": 30, "bob": 31}"#,
        r#"(map[int]string) ids = {1: "one", 2: "two"}"#,
        "(app.Point) p = (X = 3, Y = 4)",
        "(app.Color) c = Green",
        "(app.Result) r = Ok(value = 1)",
        r#"(app.Result) f = Fail(code = 2, reason = "bad")"#,
        "(app.Shape) s = app.Circle (R = 2)",
        "(app.Shape) nilShape = nil",
        r#"((int, string)) t = (1, "one")"#,
        "(bool) flag = true",
        "(char) ch = 'z'",
        "(float) ratio = 2.5",
        "(int) k = 10",
        "(func(int) int) add = app.main$lambda1 (env 0x",
        "(byte) b = 7",
        "(any) a = int 42",
        "(int) nums[1] = 3",
        "(app.Point) *pp = (X = 1, Y = 2)",
        r#"(weak *app.Node) child->parent = -> (name = "root", parent = nil)"#,
    ] {
        assert_has(&out, want);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn lldb_shows_fallible_results_and_functions() {
    let Some(lldb) = lldb() else { return };
    let src = fixture("values.zl");
    let dir = tmpdir("funcs");
    let exe = dir.join("values");
    build(&src, &exe, &[]);
    let out = run_lldb(
        &lldb,
        &exe,
        &[
            format!("b values.zl:{}", marker(&src, "parse_ret")),
            "b app.Point.Length".into(),
            format!("b values.zl:{}", marker(&src, "generic")),
            "run".into(),
            "frame variable".into(),
            "continue".into(),
            "frame variable".into(),
            "continue".into(),
            "bt 6".into(),
            "frame variable this".into(),
            "continue".into(),
            "bt 1".into(),
            "frame variable a b".into(),
            "continue".into(),
            "bt 1".into(),
            "frame variable a".into(),
        ],
    );
    // The result slot `~r0` at the closing brace (`return error(...)`, then `return len(s)`).
    assert_has(&out, r#"(string) s = """#);
    assert_has(&out, r#"(int!) ~r0 = error: "empty string""#);
    assert_has(&out, "(int!) ~r0 = 3");
    // Methods by name, after the prologue (the receiver is already stored).
    assert_has(&out, "values`app.Point.Length(this=0x");
    assert_has(&out, "values`app.main at values.zl:");
    assert!(!out.contains("(app.Point *) this = NULL"), "breakpoint before the prologue:\n{out}");
    // Generic instances with readable names.
    assert_has(&out, "values`app.Max<int>(a=9, b=3)");
    assert_has(&out, "(int) a = 9");
    assert_has(&out, r#"values`app.Max<string>(a="b", b="a")"#);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn lldb_async_locals_survive_await() {
    let Some(lldb) = lldb() else { return };
    let src = fixture("async.zl");
    let dir = tmpdir("async");
    let exe = dir.join("async");
    build(&src, &exe, &[]);
    let out = run_lldb(
        &lldb,
        &exe,
        &[
            format!("b async.zl:{}", marker(&src, "after_await")),
            format!("b async.zl:{}", marker(&src, "tasks")),
            "run".into(),
            "frame variable".into(),
            "bt 6".into(),
            "continue".into(),
            "frame variable w t".into(),
        ],
    );
    // Stopped in the resume function, on a worker thread, after the await.
    assert_has(&out, r#"async`app.Work(label="work", n=4) at async.zl:"#);
    assert_has(&out, "worker_main");
    for want in [r#"(string) label = "work""#, "(int) n = 4", "(int) before = 5", "(int) sq = 16", "(Task<int>) w = done: 21"] {
        assert_has(&out, want);
    }
    assert!(out.contains("(Task<int!>) t = pending") || out.contains(r#"(Task<int!>) t = done: error: "odd""#), "task t:\n{out}");
    let _ = std::fs::remove_dir_all(dir);
}

/// `next` over an `await` that suspends: lldb's own step-over follows the
/// native stack and stops in the caller of the coroutine (a known stage-1
/// limitation, see docs/DEBUGGER.md); the formatters' `zell-next` command
/// continues to the next line of the coroutine instead, on whichever worker
/// resumes it.
#[test]
fn lldb_step_over_await() {
    let Some(lldb) = lldb() else { return };
    let src = fixture("async.zl");
    let dir = tmpdir("step");
    let exe = dir.join("async");
    build(&src, &exe, &[]);
    let before = marker(&src, "before_await");
    let after = marker(&src, "after_await");
    let plain = run_lldb(&lldb, &exe, &[format!("b async.zl:{before}"), "run".into(), "breakpoint disable".into(), "next".into(), "frame info".into()]);
    let landed_in_caller = plain.contains("async`app.main at async.zl:");
    let landed_after = plain.contains(&format!("async`app.Work(label=\"work\", n=4) at async.zl:{after}"));
    assert!(landed_in_caller || landed_after, "plain next:\n{plain}");
    if landed_in_caller {
        eprintln!("note: lldb `next` over a suspending await stops in the caller (expected until the native debugger, stage 2)");
    }
    let out = run_lldb(&lldb, &exe, &[format!("b async.zl:{before}"), "run".into(), "breakpoint disable".into(), "zell-next".into(), "frame info".into(), "frame variable sq before".into()]);
    assert_has(&out, &format!("async`app.Work(label=\"work\", n=4) at async.zl:{after}"));
    assert_has(&out, "(int) sq = 16");
    assert_has(&out, "(int) before = 5");
    let _ = std::fs::remove_dir_all(dir);
}

/// The examples named in the stage-1 plan, built as `zell build` does by
/// default (debug info on), stopped at lines found by their text.
#[test]
fn lldb_on_examples() {
    let Some(lldb) = lldb() else { return };
    let dir = tmpdir("examples");
    let cases: &[(&str, &str, &[&str])] = &[
        ("slices_maps", r#"println("nums:", nums"#, &["([]int) nums = [5, 3, 8]"]),
        ("enums", r#"println("color:", ColorName(c))"#, &["(examples.enums.Color) c = Green"]),
        ("structs", "var q = Point{X: 1.5", &["(examples.structs.Point) p = (X = 4, Y = 5)"]),
        ("errors", "func Half(n int) int! {", &[]),
        ("generics", "func Max<T: Ordered>(a, b T) T {", &[]),
        ("functions", "func Apply(f func(int) int, x int) int {", &["(func(int) int) f = "]),
        ("async", "async func Square(n int, delayMs int) int {", &["(int) n = "]),
    ];
    for (name, needle, wants) in cases {
        let src = repo().join("examples").join(format!("{name}.zl"));
        let exe = dir.join(name);
        build(&src, &exe, &[]);
        // Function headers: break on the first statement of the function.
        let line = line_with(&src, needle);
        let bp = if needle.starts_with("func ") || needle.starts_with("async func ") { line + 1 } else { line };
        let out = run_lldb(&lldb, &exe, &[format!("b {name}.zl:{bp}"), "run".into(), "frame variable".into(), "bt 1".into()]);
        assert!(out.contains("stop reason = breakpoint"), "{name}: breakpoint at line {bp} not hit:\n{out}");
        assert!(!out.contains("<unreadable>") && !out.contains("<invalid>"), "{name}: unreadable values:\n{out}");
        for want in *wants {
            assert_has(&out, want);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// `llvm-dwarfdump --verify` accepts the DWARF of every example and fixture.
#[test]
fn dwarfdump_verifies_all_examples() {
    let Some(dd) = tool(&["llvm-dwarfdump", "llvm-dwarfdump-18"]) else {
        eprintln!("skipped: llvm-dwarfdump is not installed");
        return;
    };
    let dir = tmpdir("dwarf");
    let mut srcs: Vec<PathBuf> = std::fs::read_dir(repo().join("examples")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|x| x == "zl")).collect();
    srcs.push(repo().join("examples/multi"));
    srcs.push(fixture("values.zl"));
    srcs.push(fixture("async.zl"));
    srcs.sort();
    for src in &srcs {
        let name = src.file_stem().unwrap().to_string_lossy().to_string();
        let obj = dir.join(format!("{name}.o"));
        build(src, &obj, &["--emit=obj"]);
        let o = Command::new(&dd).arg("--verify").arg(&obj).output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(o.status.success() && out.contains("No errors."), "llvm-dwarfdump --verify {name}:\n{out}{}", String::from_utf8_lossy(&o.stderr));
        // One compile unit per source file, DWARF 5.
        let info = Command::new(&dd).arg("--debug-info").arg(&obj).output().unwrap();
        let info = String::from_utf8_lossy(&info.stdout);
        assert!(info.contains("version = 0x0005"), "{name}: not DWARF 5");
        assert!(info.contains("DW_TAG_compile_unit") && info.contains("DW_TAG_subprogram"), "{name}: no CU/subprograms");
    }
    let _ = std::fs::remove_dir_all(dir);
}

fn disasm(objdump: &str, obj: &Path) -> String {
    let o = Command::new(objdump).arg("-d").arg("--no-show-raw-insn").arg(obj).output().unwrap();
    assert!(o.status.success());
    // Drop the header line naming the file.
    String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.contains("file format")).collect::<Vec<_>>().join("\n")
}

fn has_section(obj: &Path, name: &str) -> bool {
    let bytes = std::fs::read(obj).unwrap();
    bytes.windows(name.len()).any(|w| w == name.as_bytes())
}

/// Debug info is on by default, off in `--release`, back with `--release
/// -g`, and removed by `--strip`; it never changes the generated code
/// (except the keep-alive loads of unoptimised coroutines, not used here).
#[test]
fn debug_info_flags_and_identical_code() {
    let dir = tmpdir("flags");
    let src = repo().join("benchmarks/fib/main.zl");
    let src = if src.is_file() { src } else { fixture("values.zl") };
    let dbg = dir.join("dbg.o");
    let rel = dir.join("rel.o");
    let relg = dir.join("relg.o");
    let strip = dir.join("strip.o");
    build(&src, &dbg, &["--emit=obj"]);
    build(&src, &rel, &["--emit=obj", "--release"]);
    build(&src, &relg, &["--emit=obj", "--release", "-g"]);
    build(&src, &strip, &["--emit=obj", "--strip"]);
    // ".debug_info" in ELF and COFF, "__debug_info" (segment __DWARF) in Mach-O
    assert!(has_section(&dbg, "debug_info"), "debug build without DWARF");
    assert!(has_section(&relg, "debug_info"), "--release -g without DWARF");
    // Objects always carry DWARF line tables where `.zell_lines` is built
    // from them after the link (839dc36, ABI.md 8.2): without -g they are
    // line tables only, so no variable is described (`nums` is a local of
    // the values.zl fixture); the linked release executable drops the
    // DWARF (checked below).
    let local = |o: &Path| std::fs::read(o).unwrap().windows(5).any(|w| w == b"nums\0");
    if src.ends_with("values.zl") {
        assert!(local(&dbg) && local(&relg), "-g objects without variables");
        assert!(!local(&rel), "release object with full DWARF");
        assert!(!local(&strip), "--strip object with full DWARF");
    }
    if !zell::lines::supported(&zell::target::current()) {
        assert!(!has_section(&rel, "debug_info"), "release build with DWARF");
        assert!(!has_section(&strip, "debug_info"), "--strip kept DWARF");
    }
    if let Some(objdump) = tool(&["llvm-objdump", "llvm-objdump-18"]) {
        assert_eq!(disasm(&objdump, &rel), disasm(&objdump, &relg), "-g changed the release code of {}", src.display());
        assert_eq!(disasm(&objdump, &dbg), disasm(&objdump, &strip), "debug info changed the -O0 code of {}", src.display());
    } else {
        eprintln!("skipped code comparison: llvm-objdump is not installed");
    }
    // Executables: release is stripped, `--release -g` keeps the symbols.
    let exe = dir.join("relg");
    build(&src, &exe, &["--release", "-g"]);
    assert!(has_section(&exe, ".debug_info"));
    let exe = dir.join("rel");
    build(&src, &exe, &["--release"]);
    assert!(!has_section(&exe, ".debug_line"));
    let _ = std::fs::remove_dir_all(dir);
}

/// `zell debug --lldb` builds with debug info and loads the formatters.
#[test]
fn zell_debug_subcommand() {
    let Some(_) = lldb() else { return };
    let dir = tmpdir("subcmd");
    let src = fixture("values.zl");
    let script = dir.join("cmds.lldb");
    std::fs::write(&script, format!("b values.zl:{}\nrun\nframe variable nums name\n", marker(&src, "values"))).unwrap();
    ensure_runtime();
    let o = Command::new(zell()).arg("debug").arg("--lldb").arg("--batch").arg("--source").arg(&script).arg(&src).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert_has(&out, "([]int) nums = [5, 3, 8]");
    assert_has(&out, r#"(string) name = "zell""#);
    let p = Command::new(zell()).args(["debug", "--formatters", "lldb"]).output().unwrap();
    let path = String::from_utf8_lossy(&p.stdout).trim().to_string();
    assert!(Path::new(&path).is_file(), "formatters path {path:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn gdb_pretty_printers() {
    let Some(gdb) = tool(&["gdb"]) else {
        eprintln!("skipped: gdb is not installed");
        return;
    };
    let src = fixture("values.zl");
    let dir = tmpdir("gdb");
    let exe = dir.join("values");
    build(&src, &exe, &[]);
    let o = Command::new(&gdb)
        .args(["--batch", "-nx", "-ex"])
        .arg(format!("source {}", repo().join("tools/gdb/zell_printers.py").display()))
        .arg("-ex")
        .arg(format!("break values.zl:{}", marker(&src, "values")))
        .args(["-ex", "run", "-ex", "info locals", "-ex", "print child->parent"])
        .arg(&exe)
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    if out.contains("ptrace") || !out.contains("Breakpoint 1,") {
        eprintln!("skipped: gdb cannot run the program here:\n{out}{}", String::from_utf8_lossy(&o.stderr));
        return;
    }
    for want in [
        r#"name = "zell""#,
        "nums = []int len 3 = {5, 3, 8}",
        r#"["alice"] = 30"#,
        r#"["bob"] = 31"#,
        "r = Ok(value = 1)",
        r#"f = Fail(code = 2, reason = "bad")"#,
        "s = app.Circle (R = 2)",
        "nilShape = nil",
        "ch = 'z'",
        "add = app.main$lambda1 (env 0x",
        "a = int 42",
        r#"-> (name = "root", parent = nil)"#,
    ] {
        assert!(out.contains(want), "expected {want:?} in gdb output:\n{out}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// lldb-dap (the adapter behind the VS Code `zell` debug type)
// ---------------------------------------------------------------------------

struct Dap {
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<serde_json::Value>,
    seq: i64,
    child: std::process::Child,
}

impl Dap {
    fn start(exe: &str) -> Dap {
        let mut child = Command::new(exe).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(n) = line.strip_prefix("Content-Length:") {
                        len = n.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                if r.read_exact(&mut body).is_err() {
                    return;
                }
                if let Ok(v) = serde_json::from_slice(&body) {
                    if tx.send(v).is_err() {
                        return;
                    }
                }
            }
        });
        let stdin = child.stdin.take().unwrap();
        Dap { stdin, rx, seq: 0, child }
    }

    fn send(&mut self, command: &str, args: serde_json::Value) -> i64 {
        self.seq += 1;
        let msg = serde_json::json!({"seq": self.seq, "type": "request", "command": command, "arguments": args});
        let body = msg.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
        self.seq
    }

    /// Waits for a message matching `pred` (30 s).
    fn wait(&self, what: &str, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(v) if pred(&v) => return v,
                Ok(_) => {}
                Err(_) => panic!("lldb-dap: timed out waiting for {what}"),
            }
        }
    }

    fn request(&mut self, command: &str, args: serde_json::Value) -> serde_json::Value {
        let seq = self.send(command, args);
        let r = self.wait(command, |v| v["type"] == "response" && v["request_seq"] == seq);
        assert_eq!(r["success"], true, "lldb-dap {command} failed: {r}");
        r
    }
}

impl Drop for Dap {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn lldb_dap_launch_shows_formatted_locals() {
    // Found on PATH, not run: `lldb-dap --version` waits for a DAP client.
    let on_path = |n: &str| std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(n)).find(|c| c.is_file()));
    let found = ["lldb-dap", "lldb-dap-18", "lldb-vscode", "lldb-vscode-18"].iter().find_map(|n| on_path(n)).or_else(|| Some(PathBuf::from("/usr/lib/llvm-18/bin/lldb-dap")).filter(|p| p.is_file()));
    let Some(dap) = found.map(|p| p.to_string_lossy().to_string()) else {
        eprintln!("skipped: lldb-dap is not installed");
        return;
    };
    let src = fixture("values.zl");
    let dir = tmpdir("dap");
    let exe = dir.join("values");
    build(&src, &exe, &[]);
    let mut d = Dap::start(&dap);
    d.request("initialize", serde_json::json!({"adapterID": "zell", "clientID": "zell-test", "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path"}));
    // What the VS Code extension sends for `type: "zell"`, `request: "launch"`.
    d.request(
        "launch",
        serde_json::json!({
            "program": exe,
            "args": [],
            "env": ["ZELL_DEBUG_TEST=1"],
            "cwd": dir,
            "initCommands": [format!("command script import \"{}\"", formatters().display())],
        }),
    );
    d.wait("initialized", |v| v["type"] == "event" && v["event"] == "initialized");
    let r = d.request("setBreakpoints", serde_json::json!({"source": {"path": src}, "breakpoints": [{"line": marker(&src, "values")}]}));
    assert_eq!(r["body"]["breakpoints"][0]["verified"], true, "breakpoint not verified: {r}");
    d.request("configurationDone", serde_json::json!({}));
    let stopped = d.wait("stopped", |v| v["type"] == "event" && v["event"] == "stopped");
    let tid = stopped["body"]["threadId"].clone();
    let st = d.request("stackTrace", serde_json::json!({"threadId": tid, "levels": 1}));
    let frame = &st["body"]["stackFrames"][0];
    assert_eq!(frame["name"], "app.main", "top frame: {frame}");
    let scopes = d.request("scopes", serde_json::json!({"frameId": frame["id"]}));
    let locals = scopes["body"]["scopes"][0]["variablesReference"].clone();
    let vars = d.request("variables", serde_json::json!({"variablesReference": locals}));
    let get = |name: &str| -> String {
        vars["body"]["variables"].as_array().unwrap().iter().find(|v| v["name"] == name).map(|v| v["value"].as_str().unwrap_or("").to_string()).unwrap_or_else(|| panic!("no local {name}: {vars}"))
    };
    assert_eq!(get("name"), r#""zell""#);
    assert_eq!(get("nums"), "[5, 3, 8]");
    assert_eq!(get("r"), "Ok(value = 1)");
    assert_eq!(get("ages"), r#"{"alice": 30, "bob": 31}"#);
    // Expanding a slice gives its elements (synthetic children).
    let nums = vars["body"]["variables"].as_array().unwrap().iter().find(|v| v["name"] == "nums").unwrap()["variablesReference"].clone();
    let elems = d.request("variables", serde_json::json!({"variablesReference": nums}));
    let values: Vec<&str> = elems["body"]["variables"].as_array().unwrap().iter().map(|v| v["value"].as_str().unwrap_or("")).collect();
    assert_eq!(values, ["5", "3", "8"]);
    let _ = d.send("disconnect", serde_json::json!({"terminateDebuggee": true}));
    let _ = std::fs::remove_dir_all(dir);
}
