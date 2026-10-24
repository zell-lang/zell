//! The in-process debug agent (docs/DEBUGGER.md stage 2): programs built
//! with `--debug-agent` and started with `ZELL_DEBUG=127.0.0.1:0,wait` are
//! driven over the Debug Adapter Protocol with `zell::dap::Client`.
//!
//! Fixtures: `tests/fixtures/debug_agent/*.zl` and the stage-1 fixtures
//! `tests/fixtures/debug/{values,async}.zl`, with `// BREAK <name>` markers
//! on the lines the tests stop at.

use serde_json::{json, Value};
use zell::dap::Client;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(30);

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn fixture(rel: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(rel);
    std::fs::canonicalize(&p).unwrap_or(p)
}

fn ensure_runtime() {
    let lib = repo().join("runtime/libzell_rt.a");
    if !lib.is_file() {
        let st = Command::new("make").arg("-C").arg(repo().join("runtime")).status().expect("cannot run make");
        assert!(st.success(), "make -C runtime failed");
    }
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zell-dbga-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Builds `src` into `out` with extra flags.
fn build(src: &Path, out: &Path, flags: &[&str]) {
    ensure_runtime();
    let r = Command::new(env!("CARGO_BIN_EXE_zell")).arg("build").arg(src).arg("-o").arg(out).args(flags).output().unwrap();
    assert!(r.status.success(), "build of {} failed:\n{}", src.display(), String::from_utf8_lossy(&r.stderr));
}

/// The line of `// BREAK name` in `src`.
fn line_of(src: &Path, name: &str) -> i64 {
    let text = std::fs::read_to_string(src).unwrap();
    let marker = format!("// BREAK {name}");
    text.lines().position(|l| l.trim_end().ends_with(&marker)).unwrap_or_else(|| panic!("no marker {marker} in {}", src.display())) as i64 + 1
}

/// A debugged program.
struct Session {
    src: PathBuf,
    child: Child,
    c: Client,
}

impl Session {
    fn start(rel: &str, flags: &[&str]) -> Session {
        let src = fixture(rel);
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let stem = rel.replace(['/', '.'], "_");
        // One directory per session: tests sharing a fixture run in parallel.
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let exe = tmpdir(&format!("{stem}-{n}")).join(&stem);
        build(&src, &exe, flags);
        let (child, addr) = zell::dap::spawn(&exe, &[], 0, true, true).expect("agent did not start");
        let c = Client::connect(&addr, T).expect("cannot connect to the agent");
        Session { src, child, c }
    }

    /// initialize + attach; `bps` (marker names, with optional condition /
    /// log message) are set before configurationDone.
    fn begin(&mut self, bps: &[(&str, &str, &str)]) -> Value {
        let src = self.src.clone();
        let lines: Vec<Value> = bps
            .iter()
            .map(|(m, cond, log)| {
                let mut b = json!({"line": line_of(&src, m)});
                if !cond.is_empty() {
                    b["condition"] = json!(cond);
                }
                if !log.is_empty() {
                    b["logMessage"] = json!(log);
                }
                b
            })
            .collect();
        let path = src.to_string_lossy().to_string();
        self.c
            .handshake(|c| {
                if !lines.is_empty() {
                    let r = c.request("setBreakpoints", json!({"source": {"path": path}, "breakpoints": lines}))?;
                    for b in r["breakpoints"].as_array().unwrap() {
                        assert_eq!(b["verified"], true, "breakpoint not verified: {b}");
                    }
                }
                Ok(())
            })
            .expect("handshake failed")
    }

    fn stopped(&mut self) -> Value {
        match self.c.wait_event("stopped", T) {
            Ok(e) => e["body"].clone(),
            Err(e) => panic!("no stopped event ({e}); transcript:\n{}", self.c.transcript.join("\n")),
        }
    }

    /// The top frame of task `t`: (id, function, line).
    fn top(&mut self, t: i64) -> (i64, String, i64) {
        let fs = self.c.stack(t).unwrap();
        let f = &fs[0];
        (f["id"].as_i64().unwrap(), f["name"].as_str().unwrap().to_string(), f["line"].as_i64().unwrap())
    }

    /// name -> (type, value) of the top frame's locals of task `t`.
    fn vars(&mut self, t: i64) -> Vec<(String, String, String, i64)> {
        let (f, _, _) = self.top(t);
        self.c.locals(f).unwrap().iter().map(|v| (v["name"].as_str().unwrap().to_string(), v["type"].as_str().unwrap().to_string(), v["value"].as_str().unwrap().to_string(), v["variablesReference"].as_i64().unwrap())).collect()
    }

    fn eval(&mut self, t: i64, expr: &str) -> Result<String, String> {
        let (f, _, _) = self.top(t);
        self.c.request("evaluate", json!({"expression": expr, "frameId": f, "context": "watch"})).map(|r| r["result"].as_str().unwrap_or("").to_string())
    }

    /// Continues and expects the program to end; returns its stdout.
    fn finish(mut self) -> String {
        self.c.request("continue", json!({"threadId": 1})).unwrap();
        self.c.wait_event("terminated", T).expect("program did not terminate");
        let out = self.child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

fn find<'a>(vs: &'a [(String, String, String, i64)], name: &str) -> &'a (String, String, String, i64) {
    vs.iter().find(|v| v.0 == name).unwrap_or_else(|| panic!("no variable {name} in {vs:?}"))
}

#[test]
fn breakpoint_stops_with_variables() {
    let mut s = Session::start("debug/values.zl", &[]);
    let caps = s.begin(&[("values", "", "")]);
    assert_eq!(caps["supportsConditionalBreakpoints"], true);
    let ev = s.stopped();
    assert_eq!(ev["reason"], "breakpoint");
    let t = ev["threadId"].as_i64().unwrap();
    let line = line_of(&s.src, "values");
    let (_, func, l) = s.top(t);
    assert_eq!((func.as_str(), l), ("app.main", line));
    let vs = s.vars(t);
    let check = |name: &str, ty: &str, val: &str| {
        let v = find(&vs, name);
        assert_eq!((v.1.as_str(), v.2.as_str()), (ty, val), "variable {name}");
    };
    check("name", "string", "\"zell\"");
    check("empty", "string", "\"\"");
    check("nums", "[]int", "len 3, cap 3: [5 3 8]");
    check("none", "[]int", "len 0, cap 0: []");
    check("p", "Point", "Point{X: 3, Y: 4}");
    check("pp", "*Point", "*Point{X: 1, Y: 2}");
    check("c", "Color", "Color.Green");
    check("r", "Result", "Ok(value: 1)");
    check("f", "Result", "Fail(code: 2, reason: \"bad\")");
    check("s", "Shape", "Circle(Circle{R: 2})");
    check("nilShape", "Shape", "nil");
    check("x", "int", "-1");
    check("y", "int", "3");
    check("flag", "bool", "true");
    check("ch", "char", "'z'");
    check("ratio", "float", "2.5");
    check("arr", "[3]int", "[0 2 0]");
    check("b", "byte", "7");
    check("a", "any", "int(42)");
    check("parent", "*Node", "*Node{name: \"root\", parent: nil}");
    let ages = find(&vs, "ages").clone();
    assert_eq!(ages.1, "map[string]int");
    assert!(ages.2.contains("\"bob\":31") && ages.2.contains("\"alice\":30"), "{}", ages.2);
    // Expandable children: slice elements (paged), map entries, struct fields.
    let nums = find(&vs, "nums").3;
    let kids = s.c.request("variables", json!({"variablesReference": nums, "start": 1, "count": 2})).unwrap();
    let names: Vec<(&str, &str)> = kids["variables"].as_array().unwrap().iter().map(|v| (v["name"].as_str().unwrap(), v["value"].as_str().unwrap())).collect();
    assert_eq!(names, [("[1]", "3"), ("[2]", "8")]);
    let m = s.c.children(ages.3).unwrap();
    assert!(m.iter().any(|v| v["name"] == "[\"bob\"]" && v["value"] == "31"), "{m:?}");
    let pk = s.c.children(find(&vs, "pp").3).unwrap();
    assert_eq!(pk.iter().map(|v| v["name"].as_str().unwrap()).collect::<Vec<_>>(), ["X", "Y"]);
    // evaluate v1: paths and comparisons.
    assert_eq!(s.eval(t, "nums[2]").unwrap(), "8");
    assert_eq!(s.eval(t, "ages[\"alice\"]").unwrap(), "30");
    assert_eq!(s.eval(t, "pp.Y").unwrap(), "2");
    assert_eq!(s.eval(t, "child.parent.name").unwrap(), "\"root\"");
    assert_eq!(s.eval(t, "p.X > 2.5 && len(nums) == 3").unwrap(), "true");
    assert_eq!(s.eval(t, "c == Green").unwrap(), "true");
    assert_eq!(s.eval(t, "nilShape == nil").unwrap(), "true");
    assert!(s.eval(t, "missing + 1").is_err());
    let out = s.finish();
    assert!(out.starts_with("zell "), "{out}");
}

#[test]
fn generic_instance_locals() {
    let mut s = Session::start("debug_agent/generic.zl", &[]);
    s.begin(&[("wrap", "", "")]);
    let t = s.stopped()["threadId"].as_i64().unwrap();
    let vs = s.vars(t);
    assert_eq!(find(&vs, "v").2, "42");
    assert_eq!(find(&vs, "b").2, "Box<int>{Value: 42, Count: 1}");
    s.c.request("continue", json!({"threadId": t})).unwrap();
    let t = s.stopped()["threadId"].as_i64().unwrap();
    let vs = s.vars(t);
    assert_eq!(find(&vs, "v").2, "\"hi\"");
    assert_eq!(find(&vs, "b").1, "Box<string>");
    assert_eq!(s.finish(), "42 hi 3\n");
}

#[test]
fn error_values_and_step_out() {
    let mut s = Session::start("debug/values.zl", &[]);
    s.begin(&[("parse_ret", "", "")]);
    // Stopped at the closing brace of Parse (every return passes it): the
    // failing call first, with its result `~r0`-like locals still there.
    let t = s.stopped()["threadId"].as_i64().unwrap();
    let (_, func, _) = s.top(t);
    assert_eq!(func, "app.Parse");
    assert_eq!(find(&s.vars(t), "s").2, "\"\"");
    s.c.request("continue", json!({"threadId": t})).unwrap();
    let t = s.stopped()["threadId"].as_i64().unwrap();
    let vs = s.vars(t);
    assert_eq!(find(&vs, "s").2, "\"abc\"");
    // The caller's view of the first call's error.
    let up = s.c.stack(t).unwrap()[1]["id"].as_i64().unwrap();
    let main = s.c.locals(up).unwrap();
    assert!(main.iter().any(|v| v["name"] == "x" && v["value"] == "-1"), "{main:?}");
    s.c.request("stepOut", json!({"threadId": t})).unwrap();
    let ev = s.stopped();
    assert_eq!(ev["reason"], "step");
    let (_, func, _) = s.top(t);
    assert_eq!(func, "app.main");
    s.finish();
}

#[test]
fn next_over_await_stays_in_task() {
    let mut s = Session::start("debug/async.zl", &[]);
    s.begin(&[("before_await", "", "")]);
    let ev = s.stopped();
    let t = ev["threadId"].as_i64().unwrap();
    let (_, func, line) = s.top(t);
    assert_eq!((func.as_str(), line), ("app.Work", line_of(&s.src, "before_await")));
    // Before its first suspension an async function runs as part of the
    // caller's task (ABI.md 10.3): Work is still on main's thread here,
    // while Half, which already suspended, has a task of its own.
    let threads = s.c.request("threads", json!({})).unwrap();
    let names: Vec<String> = threads["threads"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().to_string()).collect();
    assert!(threads["threads"].as_array().unwrap().iter().any(|x| x["id"] == t && x["name"] == "main"), "{threads}");
    assert!(names.iter().any(|n| n.contains("Half")) && !names.iter().any(|n| n.contains("Work")), "{threads}");
    s.c.request("next", json!({"threadId": t})).unwrap();
    // The await suspends: Work gets its task there and the step ends in it.
    // The task takes the record Work was observed under, so the logical
    // thread keeps its id (docs/DEBUGGER.md, "task ids").
    let ev = s.stopped();
    assert_eq!(ev["reason"].as_str(), Some("step"));
    assert_eq!(ev["threadId"].as_i64(), Some(t), "the task changed its id across the await: {ev}");
    let threads = s.c.request("threads", json!({})).unwrap();
    assert!(threads["threads"].as_array().unwrap().iter().any(|x| x["id"] == t && x["name"].as_str().unwrap().contains("Work")), "{threads}");
    let (_, func, line) = s.top(t);
    assert_eq!((func.as_str(), line), ("app.Work", line_of(&s.src, "after_await")));
    let vs = s.vars(t);
    assert_eq!(find(&vs, "sq").2, "16");
    assert_eq!(find(&vs, "before").2, "5");
    // When Work completes, main, which awaits it, resumes under the same id.
    let path = s.src.to_string_lossy().to_string();
    let line = line_of(&s.src, "tasks");
    s.c.request("setBreakpoints", json!({"source": {"path": path}, "breakpoints": [{"line": line}]})).unwrap();
    s.c.request("continue", json!({"threadId": t})).unwrap();
    let ev = s.stopped();
    assert_eq!(ev["threadId"].as_i64(), Some(t), "main came back under another id: {ev}");
    let (_, func, _) = s.top(t);
    assert_eq!(func, "app.main");
    let threads = s.c.request("threads", json!({})).unwrap();
    assert!(threads["threads"].as_array().unwrap().iter().any(|x| x["id"] == t && x["name"] == "main"), "{threads}");
    let out = s.finish();
    assert_eq!(out, "work 21\n21 -1\n");
}

/// An async function that already has a task of its own keeps its id
/// across every later await (finding 1a), and a step over an await whose
/// callee suspends ends in the same thread.
#[test]
fn task_id_stable_across_awaits() {
    let mut s = Session::start("debug_agent/await_ids.zl", &[]);
    s.begin(&[("first", "", "")]);
    let t = s.stopped()["threadId"].as_i64().unwrap();
    for marker in ["second", "third", "fourth"] {
        s.c.request("next", json!({"threadId": t})).unwrap();
        let ev = s.stopped();
        assert_eq!(ev["threadId"].as_i64(), Some(t), "{marker}: {ev}");
        let (_, func, line) = s.top(t);
        assert_eq!((func.as_str(), line), ("app.Loop", line_of(&s.src, marker)), "{marker}");
    }
    assert_eq!(s.finish(), "loop 6\n");
}

fn set_var(s: &mut Session, r: i64, name: &str, value: &str) -> Result<Value, String> {
    s.c.request("setVariable", json!({"variablesReference": r, "name": name, "value": value}))
}

#[test]
fn set_variable_on_scalars() {
    let mut s = Session::start("debug_agent/setvar.zl", &[]);
    let caps = s.begin(&[("set", "", "")]);
    assert_eq!(caps["supportsSetVariable"], true);
    let t = s.stopped()["threadId"].as_i64().unwrap();
    let (f, _, _) = s.top(t);
    let scopes = s.c.request("scopes", json!({"frameId": f})).unwrap();
    let locals = scopes["scopes"][0]["variablesReference"].as_i64().unwrap();
    let quoted = r#""new \"one\"""#;
    for (name, value) in [("n", "42"), ("small", "-7"), ("ratio", "1.25"), ("ok", "true"), ("name", quoted), ("ch", "'z'")] {
        let r = set_var(&mut s, locals, name, value).unwrap_or_else(|e| panic!("setVariable {name}: {e}"));
        assert_eq!(r["value"].as_str(), Some(value), "{name}");
    }
    // Out of range and wrong types are refused.
    assert!(set_var(&mut s, locals, "small", "300").is_err());
    assert!(set_var(&mut s, locals, "ok", "maybe").is_err());
    assert!(set_var(&mut s, locals, "nums", "[1]").is_err());
    assert!(set_var(&mut s, locals, "missing", "1").is_err());
    // Fields and elements through their parent's reference.
    let vs = s.vars(t);
    let p = find(&vs, "p").3;
    set_var(&mut s, p, "X", "9").unwrap();
    set_var(&mut s, p, "Y", "-0.5").unwrap();
    let nums = find(&vs, "nums").3;
    set_var(&mut s, nums, "[1]", "20").unwrap();
    // `variables` shows the new values.
    let vs = s.vars(t);
    for (name, want) in [("n", "42"), ("small", "-7"), ("ratio", "1.25"), ("ok", "true"), ("name", quoted), ("ch", "'z'"), ("p", "Point{X: 9, Y: -0.5}")] {
        assert_eq!(find(&vs, name).2, want, "{name}");
    }
    // The program goes on with them (the old string was released).
    assert_eq!(s.finish(), "start\n42 -7 1.25 true new \"one\" z 9 -0.5 [1 20 3]\n");
}

/// Sets a breakpoint with `cond` as hitCondition on `square` (hit n has
/// i = n - 1) and returns the setBreakpoints response.
fn hit_session(cond: &str) -> (Session, Value) {
    let mut s = Session::start("debug_agent/loop.zl", &[]);
    let path = s.src.to_string_lossy().to_string();
    let line = line_of(&s.src, "square");
    let mut resp = Value::Null;
    s.c.handshake(|c| {
        resp = c.request("setBreakpoints", json!({"source": {"path": path}, "breakpoints": [{"line": line, "hitCondition": cond}]}))?;
        Ok(())
    })
    .unwrap();
    (s, resp)
}

/// hitCondition: `== N`, `>= N`, `> N`, `< N`, `<= N`, `% N` and a bare `N`.
#[test]
fn hit_conditions() {
    for (cond, want) in [("3", vec![2]), ("== 2", vec![1]), (">= 9", vec![8, 9]), ("> 7", vec![7, 8, 9]), ("< 3", vec![0, 1]), ("<= 2", vec![0, 1]), ("% 4", vec![3, 7]), ("%3", vec![2, 5, 8])] {
        let (mut s, r) = hit_session(cond);
        assert_eq!(r["breakpoints"][0]["verified"], true);
        assert!(r["breakpoints"][0].get("message").is_none(), "{cond}: {r}");
        let mut got = vec![];
        for _ in 0..want.len() {
            let t = s.stopped()["threadId"].as_i64().unwrap();
            got.push(find(&s.vars(t), "i").2.parse::<i64>().unwrap());
            s.c.request("continue", json!({"threadId": t})).unwrap();
        }
        assert_eq!(got, want, "hitCondition {cond}");
        s.c.wait_event("terminated", T).unwrap_or_else(|e| panic!("hitCondition {cond}: an extra stop ({e}): {:?}", s.c.events));
        let _ = s.child.wait();
    }
    // A malformed condition is reported and ignored (every hit stops).
    let (mut s, r) = hit_session("every 3");
    assert!(r["breakpoints"][0]["message"].as_str().unwrap_or("").contains("hit condition"), "{r}");
    let t = s.stopped()["threadId"].as_i64().unwrap();
    assert_eq!(find(&s.vars(t), "i").2, "0");
    let _ = s.c.request("disconnect", json!({}));
    let _ = s.child.wait();
}

/// Breakpoint paths: a symlinked spelling and a relative tail bind to the
/// file in the agent's table (canonicalised by the agent, not the client).
#[cfg(unix)]
#[test]
fn breakpoint_path_spellings() {
    let mut s = Session::start("debug_agent/loop.zl", &[]);
    let dir = tmpdir("bp-paths");
    let link = dir.join("linked");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(s.src.parent().unwrap(), &link).unwrap();
    let via_link = link.join("loop.zl").to_string_lossy().to_string();
    let line = line_of(&s.src, "square");
    let want = s.src.canonicalize().unwrap().to_string_lossy().to_string();
    let vl = via_link.clone();
    s.c.handshake(|c| {
        for p in ["debug_agent/loop.zl", vl.as_str()] {
            let r = c.request("setBreakpoints", json!({"source": {"path": p}, "breakpoints": [{"line": line}]}))?;
            assert_eq!(r["breakpoints"][0]["verified"], true, "{p}: {r}");
            assert_eq!(r["breakpoints"][0]["source"]["path"].as_str(), Some(want.as_str()), "{p}: {r}");
        }
        Ok(())
    })
    .unwrap();
    let t = s.stopped()["threadId"].as_i64().unwrap();
    assert_eq!(find(&s.vars(t), "i").2, "0");
    s.c.request("setBreakpoints", json!({"source": {"path": via_link}, "breakpoints": []})).unwrap();
    assert_eq!(s.finish(), "total 285\n");
}

#[test]
fn logpoint_prints_without_stopping() {
    let mut s = Session::start("debug_agent/loop.zl", &[]);
    s.begin(&[("square", "", "sq of {i}: {i * 1}"), ("loop", "i >= 3 && i < 5", "loop i={i} total={total}")]);
    let out_wait = Instant::now();
    s.c.wait_event("terminated", T).expect("the program must run to the end");
    let events: Vec<Value> = s.c.events.drain(..).collect();
    assert!(!events.iter().any(|e| e["event"] == "stopped"), "a logpoint stopped: {events:?}");
    let output: String = events.iter().filter(|e| e["event"] == "output").map(|e| e["body"]["output"].as_str().unwrap().to_string()).collect();
    assert!(output.contains("loop i=3 total=5\n") && output.contains("loop i=4 total=14\n"), "{output}");
    assert!(!output.contains("loop i=5"), "the condition is false for i=5: {output}");
    assert!(output.contains("sq of 9: <"), "a bad expression shows its error inline: {output}");
    assert!(out_wait.elapsed() < T);
    let out = s.child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "total 285\n");
}

#[test]
fn conditional_breakpoint_stops_only_when_true() {
    let mut s = Session::start("debug_agent/loop.zl", &[]);
    s.begin(&[("square", "i == 7", "")]);
    let t = s.stopped()["threadId"].as_i64().unwrap();
    let vs = s.vars(t);
    assert_eq!(find(&vs, "i").2, "7");
    // Caller frame: its loop variable.
    let frames = s.c.stack(t).unwrap();
    assert_eq!(frames[1]["name"], "app.main");
    let up = s.c.locals(frames[1]["id"].as_i64().unwrap()).unwrap();
    assert!(up.iter().any(|v| v["name"] == "total" && v["value"] == "91"), "{up:?}");
    // No other stop: the program ends.
    assert_eq!(s.finish(), "total 285\n");
}

#[test]
fn function_breakpoint_and_step_in() {
    let mut s = Session::start("debug_agent/loop.zl", &[]);
    s.begin(&[("loop", "i == 2", "")]);
    let t = s.stopped()["threadId"].as_i64().unwrap();
    s.c.request("stepIn", json!({"threadId": t})).unwrap();
    s.stopped();
    let (_, func, line) = s.top(t);
    assert_eq!((func.as_str(), line), ("app.square", line_of(&s.src, "square")));
    s.c.request("setBreakpoints", json!({"source": {"path": s.src.to_string_lossy()}, "breakpoints": []})).unwrap();
    let r = s.c.request("setFunctionBreakpoints", json!({"breakpoints": [{"name": "square"}]})).unwrap();
    assert_eq!(r["breakpoints"][0]["verified"], true);
    s.c.request("continue", json!({"threadId": t})).unwrap();
    let ev = s.stopped();
    assert_eq!(ev["reason"], "function breakpoint");
    assert_eq!(s.eval(t, "i").unwrap(), "3");
    s.c.request("setFunctionBreakpoints", json!({"breakpoints": []})).unwrap();
    assert_eq!(s.finish(), "total 285\n");
}

/// `i` in the outermost frame (main) of task `t`.
fn main_i(s: &mut Session, t: i64) -> i64 {
    let f = s.c.stack(t).unwrap().last().unwrap()["id"].clone();
    s.c.request("evaluate", json!({"expression": "i", "frameId": f})).unwrap()["result"].as_str().unwrap().parse().unwrap()
}

#[test]
fn pause_and_continue() {
    let mut s = Session::start("debug_agent/spin.zl", &[]);
    s.begin(&[]);
    std::thread::sleep(Duration::from_millis(200));
    s.c.request("pause", json!({"threadId": 1})).unwrap();
    let ev = s.stopped();
    assert_eq!(ev["reason"], "pause");
    let t = ev["threadId"].as_i64().unwrap();
    let (_, func, _) = s.top(t);
    assert!(func == "app.main" || func == "app.work", "{func}");
    let i1: i64 = main_i(&mut s, t);
    s.c.request("continue", json!({"threadId": t})).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    s.c.request("pause", json!({"threadId": 1})).unwrap();
    let t = s.stopped()["threadId"].as_i64().unwrap();
    let i2: i64 = main_i(&mut s, t);
    assert!(i2 > i1, "the program ran between the pauses ({i1} -> {i2})");
    s.c.request("disconnect", json!({"terminateDebuggee": true})).unwrap();
    let st = s.child.wait().unwrap();
    assert!(st.success() || st.code() == Some(0), "{st:?}");
}

#[test]
fn exception_breakpoint_on_panic() {
    let mut s = Session::start("debug_agent/panic.zl", &[]);
    s.c.handshake(|c| c.request("setExceptionBreakpoints", json!({"filters": ["panic"]})).map(|_| ())).unwrap();
    let ev = s.stopped();
    assert_eq!(ev["reason"], "exception");
    assert_eq!(ev["text"], "panic: division by zero");
    let t = ev["threadId"].as_i64().unwrap();
    let (_, func, line) = s.top(t);
    assert_eq!((func.as_str(), line), ("app.divide", line_of(&s.src, "panic")));
    assert_eq!(s.eval(t, "b").unwrap(), "0");
    s.c.request("continue", json!({"threadId": t})).unwrap();
    // The resumed run is announced, and the panic ending the program is
    // reported as `exited` (code 2) and `terminated`.
    let cont = s.c.wait_event("continued", T).expect("no continued event");
    assert_eq!(cont["body"]["threadId"].as_i64(), Some(t));
    let ex = s.c.wait_event("exited", T).expect("no exited event after the panic");
    assert_eq!(ex["body"]["exitCode"], 2);
    s.c.wait_event("terminated", T).expect("no terminated event after the panic");
    let out = s.child.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("-1 2"));
    assert!(!out.status.success(), "the panic goes on after continue");
}

#[test]
fn closure_captures_and_tuples() {
    let mut s = Session::start("debug_agent/closure.zl", &[]);
    s.begin(&[("tuple", "", ""), ("lambda", "", "")]);
    let t = s.stopped()["threadId"].as_i64().unwrap();
    assert_eq!(s.top(t).2, line_of(&s.src, "tuple"));
    let vs = s.vars(t);
    let pair = find(&vs, "pair").clone();
    assert_eq!((pair.1.as_str(), pair.2.as_str()), ("(int, string)", "(3, \"rem\")"));
    let kids = s.c.children(pair.3).unwrap();
    assert_eq!(kids.iter().map(|v| (v["name"].as_str().unwrap(), v["value"].as_str().unwrap())).collect::<Vec<_>>(), [("_0", "3"), ("_1", "\"rem\"")]);
    assert_eq!(s.eval(t, "pair._1").unwrap(), "\"rem\"");
    s.c.request("continue", json!({"threadId": t})).unwrap();
    // Inside the lambda: its parameter, its local and the captured
    // variables of main (by reference: base was changed to 20).
    let t = s.stopped()["threadId"].as_i64().unwrap();
    let (_, func, line) = s.top(t);
    assert_eq!((func.as_str(), line), ("app.main$lambda1", line_of(&s.src, "lambda")));
    let vs = s.vars(t);
    assert_eq!(find(&vs, "x").2, "1");
    assert_eq!(find(&vs, "base").2, "20");
    assert_eq!(s.eval(t, "base == 20").unwrap(), "true");
    assert!(!vs.iter().any(|v| v.0 == "label"), "only what the lambda captures: {vs:?}");
    assert_eq!(s.finish(), "sum 21 3 rem\n");
}

#[test]
fn exception_breakpoint_on_error_creation() {
    let mut s = Session::start("debug_agent/panic.zl", &[]);
    s.c.handshake(|c| c.request("setExceptionBreakpoints", json!({"filters": [], "filterOptions": [{"filterId": "error", "condition": "NotFound"}]})).map(|_| ())).unwrap();
    let ev = s.stopped();
    assert_eq!(ev["reason"], "exception");
    assert!(ev["text"].as_str().unwrap().contains("not found: zzz"), "{ev}");
    let t = ev["threadId"].as_i64().unwrap();
    let (_, func, line) = s.top(t);
    assert_eq!((func.as_str(), line), ("app.lookup", line_of(&s.src, "error")));
    s.c.request("disconnect", json!({"terminateDebuggee": true})).unwrap();
    let _ = s.child.wait();
}

#[test]
fn two_tasks_step_independently() {
    let mut s = Session::start("debug_agent/tasks.zl", &[]);
    s.begin(&[("w1", "", "")]);
    let (w1, w2, w3) = (line_of(&s.src, "w1"), line_of(&s.src, "w2"), line_of(&s.src, "w3"));
    // Both tasks reach w1 (the second one is frozen there or reports its
    // own breakpoint once the first one is stopped).
    let a = s.stopped()["threadId"].as_i64().unwrap();
    assert_eq!(s.top(a).2, w1);
    let n_a: i64 = s.eval(a, "n").unwrap().parse().unwrap();
    // Step task a alone (singleThread): two lines, in its own task.
    for want in [w2, w3] {
        s.c.request("next", json!({"threadId": a, "singleThread": true})).unwrap();
        loop {
            let ev = s.stopped();
            if ev["threadId"].as_i64() == Some(a) {
                assert_eq!(ev["reason"], "step");
                break;
            }
        }
        assert_eq!(s.top(a).2, want);
    }
    assert_eq!(s.eval(a, "acc").unwrap(), (n_a * 3).to_string());
    // The other task is stopped at w1 (breakpoint), untouched by a's steps.
    let b = loop {
        let th = s.c.request("threads", json!({})).unwrap();
        let other = th["threads"].as_array().unwrap().iter().filter_map(|x| x["id"].as_i64()).find(|&id| id != a && s.c.stack(id).map(|f| !f.is_empty() && f[0]["name"] == "app.worker").unwrap_or(false));
        if let Some(b) = other {
            break b;
        }
        s.c.wait_event("stopped", T).expect("the second task never stopped");
    };
    // b may have been frozen before its breakpoint (at the `await`, when
    // a was quicker): step it there.
    let mut guard = 0;
    while s.top(b).2 != w1 {
        guard += 1;
        assert!(guard < 5, "task {b} does not reach w1");
        s.c.events.retain(|e| !(e["event"] == "stopped" && e["body"]["threadId"].as_i64() == Some(b)));
        s.c.request("next", json!({"threadId": b, "singleThread": true})).unwrap();
        while s.stopped()["threadId"].as_i64() != Some(b) {}
    }
    let n_b: i64 = s.eval(b, "n").unwrap().parse().unwrap();
    assert_ne!(n_a, n_b);
    assert_eq!(s.eval(b, "acc").unwrap(), "0");
    // (b's own breakpoint stop may still be queued.)
    s.c.events.retain(|e| !(e["event"] == "stopped" && e["body"]["threadId"].as_i64() == Some(b)));
    s.c.request("next", json!({"threadId": b, "singleThread": true})).unwrap();
    loop {
        let ev = s.stopped();
        if ev["threadId"].as_i64() == Some(b) {
            assert_eq!(ev["reason"], "step");
            break;
        }
    }
    assert_eq!(s.top(b).2, w2);
    assert_eq!(s.eval(b, "acc").unwrap(), n_b.to_string());
    // a did not move.
    assert_eq!(s.top(a).2, w3);
    s.c.request("setBreakpoints", json!({"source": {"path": s.src.to_string_lossy()}, "breakpoints": []})).unwrap();
    assert_eq!(s.finish(), "6 60\n");
}

#[test]
fn release_build_with_agent_stops() {
    let mut s = Session::start("debug_agent/loop.zl", &["--release", "--debug-agent"]);
    s.begin(&[("square", "", "")]);
    let t = s.stopped()["threadId"].as_i64().unwrap();
    let frames = s.c.stack(t).unwrap();
    assert_eq!(frames[0]["name"], "app.square");
    assert_eq!(frames[1]["name"], "app.main");
    s.c.request("setBreakpoints", json!({"source": {"path": s.src.to_string_lossy()}, "breakpoints": []})).unwrap();
    assert_eq!(s.finish(), "total 285\n");
}

#[test]
fn capability_gate() {
    let dir = tmpdir("gate");
    let f = dir.join("u.zl");
    std::fs::write(&f, "namespace app\n\nimport io\nimport unsafe\n\nfunc main() {\n    println(ReadInt(0, 0))\n}\n").unwrap();
    let r = Command::new(env!("CARGO_BIN_EXE_zell")).arg("check").arg(&f).output().unwrap();
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("needs the `unsafe` capability"), "{}", String::from_utf8_lossy(&r.stderr));
    // Reaching it by its full path without an import is gated too
    // (also with plugin).
    for (body, cap) in [
        ("import io\n\nfunc main() {\n    println(unsafe.ReadInt(0, 0))\n}\n", "unsafe"),
        ("import io\n\nfunc main() {\n    println(try plugin.rtOpen(\"x\") else 0)\n}\n", "plugin"),
    ] {
        std::fs::write(&f, format!("namespace app\n\n{body}")).unwrap();
        let r = Command::new(env!("CARGO_BIN_EXE_zell")).arg("check").arg(&f).output().unwrap();
        assert!(!r.status.success(), "{body}");
        let err = String::from_utf8_lossy(&r.stderr);
        assert!(err.contains(&format!("needs the `{cap}` capability")), "{body}: {err}");
    }
    // A package granting the capability may import it.
    let pkg = dir.join("pkg");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(pkg.join("zell.toml"), "[package]\nname = \"low\"\nversion = \"0.1.0\"\nentry = \"src/main.zl\"\n\n[capabilities]\nunsafe = true\n").unwrap();
    std::fs::write(pkg.join("src/main.zl"), "namespace low\n\nimport io\nimport unsafe\n\nfunc main() {\n    var b = [7]\n    println(len(b), CString(0))\n}\n").unwrap();
    let r = Command::new(env!("CARGO_BIN_EXE_zell")).arg("check").current_dir(&pkg).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
}

/// Hook overhead: `Fib(35)` built `--release` (no agent) against
/// `--release --debug-agent` with ZELL_DEBUG unset. Optimised builds keep a
/// hook-free copy of every function (debug_agent.rs), so the numbers must
/// be close; the bound here is loose because CI machines are noisy, the
/// measured numbers are printed (`cargo test -- --nocapture`).
#[test]
fn overhead_with_agent_off() {
    let dir = tmpdir("overhead");
    let src = fixture("debug_agent/fib.zl");
    let (plain, agent) = (dir.join("plain"), dir.join("agent"));
    build(&src, &plain, &["--release"]);
    build(&src, &agent, &["--release", "--debug-agent"]);
    let best = |exe: &Path| {
        (0..7)
            .map(|_| {
                let t = Instant::now();
                let st = Command::new(exe).env_remove("ZELL_DEBUG").output().unwrap();
                assert!(st.status.success());
                t.elapsed().as_secs_f64()
            })
            .fold(f64::MAX, f64::min)
    };
    let (mut p, mut a) = (f64::MAX, f64::MAX);
    for _ in 0..3 {
        p = p.min(best(&plain));
        a = a.min(best(&agent));
    }
    let pct = (a / p - 1.0) * 100.0;
    println!("fib(35) release: no agent {:.1} ms, agent compiled in (off) {:.1} ms ({pct:+.1}%)", p * 1000.0, a * 1000.0);
    assert!(pct < 25.0, "agent-off overhead too high: {pct:.1}%");
}
