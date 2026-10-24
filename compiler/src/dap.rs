//! A Debug Adapter Protocol client for the in-process debug agent
//! (`debug`, docs/DEBUGGER.md stage 2): the `zell debug run` /
//! `zell debug attach` command-line debugger and the tests drive programs
//! through it.
//!
//! The agent is started by the program itself (`ZELL_DEBUG=[host]:port[,wait]`)
//! and announces its port on stderr as `zell-debug: listening on HOST:PORT`.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The stderr line announcing the agent's address.
pub const LISTENING: &str = "zell-debug: listening on ";

/// A connection to an agent.
pub struct Client {
    stream: TcpStream,
    rx: Receiver<Value>,
    seq: i64,
    /// Events received while waiting for responses, oldest first.
    pub events: VecDeque<Value>,
    /// Every message sent and received, as `-> {...}` / `<- {...}` lines.
    pub transcript: Vec<String>,
}

/// Reads one `Content-Length` framed message.
fn read_message(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse::<usize>().ok();
            }
        }
    }
    let mut body = vec![0u8; len?];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

impl Client {
    /// Connects to `addr` (`host:port`), retrying until `timeout`.
    pub fn connect(addr: &str, timeout: Duration) -> std::io::Result<Client> {
        let start = Instant::now();
        let stream = loop {
            match TcpStream::connect(addr) {
                Ok(s) => break s,
                Err(e) if start.elapsed() >= timeout => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        stream.set_nodelay(true)?;
        let read = stream.try_clone()?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(read);
            while let Some(m) = read_message(&mut r) {
                if tx.send(m).is_err() {
                    break;
                }
            }
        });
        Ok(Client { stream, rx, seq: 0, events: VecDeque::new(), transcript: Vec::new() })
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        let body = v.to_string();
        self.transcript.push(format!("-> {body}"));
        write!(self.stream, "Content-Length: {}\r\n\r\n{body}", body.len()).map_err(|e| format!("debug connection: {e}"))
    }

    fn recv(&mut self, timeout: Duration) -> Result<Value, String> {
        match self.rx.recv_timeout(timeout) {
            Ok(m) => {
                self.transcript.push(format!("<- {m}"));
                Ok(m)
            }
            Err(RecvTimeoutError::Timeout) => Err("timed out waiting for the debug agent".into()),
            Err(RecvTimeoutError::Disconnected) => Err("the debug agent closed the connection".into()),
        }
    }

    /// Sends request `command` and waits for its response (events that
    /// arrive meanwhile are queued). `Err` carries the failure message.
    pub fn request(&mut self, command: &str, args: Value) -> Result<Value, String> {
        self.seq += 1;
        let seq = self.seq;
        self.send(&json!({"seq": seq, "type": "request", "command": command, "arguments": args}))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let m = self.recv(deadline.saturating_duration_since(Instant::now()))?;
            match m["type"].as_str() {
                Some("response") if m["request_seq"].as_i64() == Some(seq) => {
                    if m["success"].as_bool() == Some(true) {
                        return Ok(m["body"].clone());
                    }
                    return Err(m["message"].as_str().unwrap_or("request failed").to_string());
                }
                Some("event") => self.events.push_back(m),
                _ => {}
            }
        }
    }

    /// Waits for the next event named `name` (earlier queued events of
    /// other names stay queued).
    pub fn wait_event(&mut self, name: &str, timeout: Duration) -> Result<Value, String> {
        if let Some(i) = self.events.iter().position(|e| e["event"] == name) {
            return Ok(self.events.remove(i).unwrap());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let m = self.recv(deadline.saturating_duration_since(Instant::now()))?;
            if m["type"] == "event" {
                if m["event"] == name {
                    return Ok(m);
                }
                self.events.push_back(m);
            }
        }
    }

    /// Like [`Self::wait_event`] for any of `names`.
    pub fn wait_any(&mut self, names: &[&str], timeout: Duration) -> Result<Value, String> {
        if let Some(i) = self.events.iter().position(|e| names.iter().any(|n| e["event"] == *n)) {
            return Ok(self.events.remove(i).unwrap());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let m = self.recv(deadline.saturating_duration_since(Instant::now()))?;
            if m["type"] == "event" {
                if names.iter().any(|n| m["event"] == *n) {
                    return Ok(m);
                }
                self.events.push_back(m);
            }
        }
    }

    /// Drains messages that are already there (non-blocking) into the
    /// event queue and returns the queue.
    pub fn poll_events(&mut self) -> Vec<Value> {
        while let Ok(m) = self.rx.try_recv() {
            self.transcript.push(format!("<- {m}"));
            if m["type"] == "event" {
                self.events.push_back(m);
            }
        }
        self.events.drain(..).collect()
    }

    /// The initialize / attach handshake; `configure` sets breakpoints
    /// before `configurationDone` lets the program run.
    pub fn handshake(&mut self, configure: impl FnOnce(&mut Client) -> Result<(), String>) -> Result<Value, String> {
        let caps = self.request("initialize", json!({"clientID": "zell", "adapterID": "zell", "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path"}))?;
        self.wait_event("initialized", Duration::from_secs(10))?;
        self.request("attach", json!({}))?;
        configure(self)?;
        self.request("configurationDone", json!({}))?;
        Ok(caps)
    }

    /// The frames of task `thread`.
    pub fn stack(&mut self, thread: i64) -> Result<Vec<Value>, String> {
        let r = self.request("stackTrace", json!({"threadId": thread}))?;
        Ok(r["stackFrames"].as_array().cloned().unwrap_or_default())
    }

    /// The locals of frame `frame` (an id from [`Self::stack`]).
    pub fn locals(&mut self, frame: i64) -> Result<Vec<Value>, String> {
        let s = self.request("scopes", json!({"frameId": frame}))?;
        let r = s["scopes"][0]["variablesReference"].as_i64().unwrap_or(0);
        self.children(r)
    }

    /// The children of variables reference `r`.
    /// DAP `setVariable`: `name` (a child of reference `r`) takes the value
    /// written as `value`; returns the value as the agent now shows it.
    pub fn set_variable(&mut self, r: i64, name: &str, value: &str) -> Result<String, String> {
        let v = self.request("setVariable", json!({"variablesReference": r, "name": name, "value": value}))?;
        Ok(v["value"].as_str().unwrap_or("").to_string())
    }

    pub fn children(&mut self, r: i64) -> Result<Vec<Value>, String> {
        let v = self.request("variables", json!({"variablesReference": r}))?;
        Ok(v["variables"].as_array().cloned().unwrap_or_default())
    }
}

/// Starts `exe` with the agent listening on `port` (0 = any) and returns
/// the child and the agent's address. With `wait` the program waits for
/// `configurationDone`. Stderr after the announcement is forwarded to
/// this process's stderr.
pub fn spawn(exe: &Path, args: &[String], port: u16, wait: bool, quiet_stdout: bool) -> Result<(Child, String), String> {
    let spec = if wait { format!("127.0.0.1:{port},wait") } else { format!("127.0.0.1:{port}") };
    let mut cmd = Command::new(exe);
    cmd.args(args).env("ZELL_DEBUG", spec).env("ZELL_DEBUG_TRACE", "1").stderr(Stdio::piped());
    if quiet_stdout {
        cmd.stdout(Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stderr);
        let mut sent = false;
        let mut line = String::new();
        while r.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            if !sent {
                if let Some(a) = line.trim_end().strip_prefix(LISTENING) {
                    let _ = tx.send(a.to_string());
                    sent = true;
                    line.clear();
                    continue;
                }
            }
            eprint!("{line}");
            line.clear();
        }
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(addr) => Ok((child, addr)),
        Err(_) => {
            let _ = child.kill();
            Err(format!("{} did not start the debug agent (built without --debug-agent?)", exe.display()))
        }
    }
}

// ---------------------------------------------------------------------------
// The interactive command-line debugger
// ---------------------------------------------------------------------------

const HELP: &str = "commands:
  b file:line [if cond]   breakpoint (also `b line` in the current file, `b func`)
  log file:line message   logpoint ({expr} parts are evaluated)
  d                       delete all breakpoints
  c                       continue
  n / s / o               next, step in, step out (of the current task)
  p expr                  print a variable path (a.b[2], m[\"k\"]) or a comparison
  locals                  the current frame's variables
  set x.f[1] = value      change a local, field or element of scalar type
  bt                      stack of the current task
  tasks                   list tasks; `task N` selects one
  up / down               select a caller / callee frame
  catch panic|error [T]   stop on panics / on creation of errors (of type T)
  q                       quit (kills the program)";

struct Repl {
    c: Client,
    task: i64,
    frame: usize,
    frames: Vec<Value>,
    file: String,
    /// path -> [(line, condition, log)]
    bps: Vec<(String, Vec<(i64, String, String)>)>,
    funcs: Vec<String>,
    exc: Vec<String>,
    err_types: String,
    done: bool,
}

impl Repl {
    fn print_events(&mut self) {
        for e in self.c.poll_events() {
            self.show_event(&e);
        }
    }

    fn show_event(&mut self, e: &Value) {
        match e["event"].as_str() {
            Some("output") => print!("{}", e["body"]["output"].as_str().unwrap_or("")),
            Some("exited") => println!("[program exited with code {}]", e["body"]["exitCode"]),
            Some("terminated") => self.done = true,
            Some("stopped") => {
                self.task = e["body"]["threadId"].as_i64().unwrap_or(1);
                self.frame = 0;
                let reason = e["body"]["reason"].as_str().unwrap_or("");
                let text = e["body"]["text"].as_str().map(|t| format!(": {t}")).unwrap_or_default();
                self.frames = self.c.stack(self.task).unwrap_or_default();
                match self.frames.first().cloned() {
                    Some(f) => {
                        println!("stopped ({reason}{text}) in task {} at {}", self.task, describe_frame(&f));
                        self.show_line(&f);
                    }
                    None => println!("stopped ({reason}{text}) in task {}", self.task),
                }
            }
            _ => {}
        }
    }

    fn show_line(&mut self, f: &Value) {
        let path = f["source"]["path"].as_str().unwrap_or("");
        self.file = path.to_string();
        let line = f["line"].as_i64().unwrap_or(0);
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Some(l) = text.lines().nth((line - 1).max(0) as usize) {
                println!("{line:>5}\t{l}");
            }
        }
    }

    /// Runs (continue / step) and waits until the program stops or ends.
    fn run(&mut self, cmd: &str) {
        let r = self.c.request(cmd, json!({"threadId": self.task}));
        if let Err(e) = r {
            println!("error: {e}");
            return;
        }
        loop {
            match self.c.wait_any(&["stopped", "terminated", "exited", "output"], Duration::from_secs(3600)) {
                Ok(e) => {
                    let ev = e["event"].as_str().unwrap_or("").to_string();
                    self.show_event(&e);
                    if ev == "stopped" || ev == "terminated" {
                        break;
                    }
                }
                Err(_) => {
                    self.done = true;
                    break;
                }
            }
        }
    }

    fn set_bps(&mut self, path: &str) {
        let Some((_, list)) = self.bps.iter().find(|(p, _)| p == path) else { return };
        let bs: Vec<Value> = list
            .iter()
            .map(|(l, c, m)| {
                let mut b = json!({"line": l});
                if !c.is_empty() {
                    b["condition"] = json!(c);
                }
                if !m.is_empty() {
                    b["logMessage"] = json!(m);
                }
                b
            })
            .collect();
        match self.c.request("setBreakpoints", json!({"source": {"path": path}, "breakpoints": bs})) {
            Ok(r) => {
                for b in r["breakpoints"].as_array().cloned().unwrap_or_default() {
                    if b["verified"] == true {
                        println!("breakpoint {} at {}:{}", b["id"], path, b["line"]);
                    } else {
                        println!("breakpoint not set: {}", b["message"].as_str().unwrap_or("?"));
                    }
                }
            }
            Err(e) => println!("error: {e}"),
        }
    }

    fn add_bp(&mut self, spec: &str, log: &str) {
        let (loc, cond) = match spec.split_once(" if ") {
            Some((l, c)) => (l.trim(), c.trim().to_string()),
            None => (spec.trim(), String::new()),
        };
        let (path, line) = match loc.rsplit_once(':') {
            Some((p, l)) if l.parse::<i64>().is_ok() => (p.to_string(), l.parse().unwrap()),
            _ if loc.parse::<i64>().is_ok() => (self.file.clone(), loc.parse().unwrap()),
            _ => {
                self.funcs.push(loc.to_string());
                let bs: Vec<Value> = self.funcs.iter().map(|f| json!({"name": f})).collect();
                match self.c.request("setFunctionBreakpoints", json!({"breakpoints": bs})) {
                    Ok(r) => {
                        if let Some(b) = r["breakpoints"].as_array().and_then(|a| a.last()) {
                            if b["verified"] == true {
                                println!("breakpoint {} at {}:{}", b["id"], b["source"]["path"].as_str().unwrap_or("?"), b["line"]);
                            } else {
                                println!("breakpoint not set: {}", b["message"].as_str().unwrap_or("?"));
                            }
                        }
                    }
                    Err(e) => println!("error: {e}"),
                }
                return;
            }
        };
        let path = crate::paths::canonical(&path).map(|p| p.to_string_lossy().to_string()).unwrap_or(path);
        match self.bps.iter_mut().find(|(p, _)| *p == path) {
            Some((_, l)) => l.push((line, cond, log.to_string())),
            None => self.bps.push((path.clone(), vec![(line, cond, log.to_string())])),
        }
        self.set_bps(&path);
    }

    fn frame_id(&self) -> Option<i64> {
        self.frames.get(self.frame).and_then(|f| f["id"].as_i64())
    }

    fn print_value(&mut self, expr: &str) {
        let mut args = json!({"expression": expr, "context": "repl"});
        if let Some(f) = self.frame_id() {
            args["frameId"] = json!(f);
        }
        match self.c.request("evaluate", args) {
            Ok(r) => {
                println!("{} = {}", expr, r["result"].as_str().unwrap_or(""));
                let vr = r["variablesReference"].as_i64().unwrap_or(0);
                if vr > 0 {
                    for v in self.c.children(vr).unwrap_or_default().iter().take(32) {
                        println!("    {}: {} = {}", v["name"].as_str().unwrap_or(""), v["type"].as_str().unwrap_or(""), v["value"].as_str().unwrap_or(""));
                    }
                }
            }
            Err(e) => println!("error: {e}"),
        }
    }

    /// `set path = value`: walks `a.b[2]` through the frame's variables
    /// (children by name) and sends setVariable for the last part.
    fn set_value(&mut self, rest: &str) {
        let Some((path, value)) = rest.split_once('=') else {
            println!("usage: set name = value");
            return;
        };
        let Some(f) = self.frame_id() else {
            println!("no frame selected (the task is not stopped)");
            return;
        };
        let scopes = self.c.request("scopes", json!({"frameId": f})).unwrap_or_default();
        let mut r = scopes["scopes"][0]["variablesReference"].as_i64().unwrap_or(0);
        let mut parts: Vec<String> = Vec::new();
        for seg in path.trim().split('.') {
            let (head, idx) = seg.split_once('[').map(|(h, i)| (h, Some(i))).unwrap_or((seg, None));
            if !head.is_empty() {
                parts.push(head.to_string());
            }
            if let Some(i) = idx {
                for e in format!("[{i}").split_inclusive(']') {
                    parts.push(e.to_string());
                }
            }
        }
        let Some((last, walk)) = parts.split_last() else {
            println!("usage: set name = value");
            return;
        };
        for name in walk {
            let kids = self.c.children(r).unwrap_or_default();
            match kids.iter().find(|v| v["name"] == name.as_str()) {
                Some(v) => r = v["variablesReference"].as_i64().unwrap_or(0),
                None => {
                    println!("error: no variable {name}");
                    return;
                }
            }
        }
        match self.c.set_variable(r, last, value.trim()) {
            Ok(v) => println!("{} = {v}", path.trim()),
            Err(e) => println!("error: {e}"),
        }
    }

    fn command(&mut self, line: &str) -> bool {
        let line = line.trim();
        let (cmd, rest) = line.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((line, ""));
        match cmd {
            "" => {}
            "h" | "help" => println!("{HELP}"),
            "b" | "break" => self.add_bp(rest, ""),
            "log" => match rest.split_once(' ') {
                Some((loc, msg)) => self.add_bp(loc, msg),
                None => println!("usage: log file:line message"),
            },
            "d" | "delete" => {
                let paths: Vec<String> = self.bps.iter().map(|(p, _)| p.clone()).collect();
                for p in paths {
                    let _ = self.c.request("setBreakpoints", json!({"source": {"path": p}, "breakpoints": []}));
                }
                self.bps.clear();
                self.funcs.clear();
                let _ = self.c.request("setFunctionBreakpoints", json!({"breakpoints": []}));
            }
            "c" | "continue" => self.run("continue"),
            "n" | "next" => self.run("next"),
            "s" | "step" => self.run("stepIn"),
            "o" | "out" | "finish" => self.run("stepOut"),
            "p" | "print" => self.print_value(rest),
            "set" => self.set_value(rest),
            "locals" => match self.frame_id() {
                Some(f) => {
                    for v in self.c.locals(f).unwrap_or_default() {
                        println!("{}: {} = {}", v["name"].as_str().unwrap_or(""), v["type"].as_str().unwrap_or(""), v["value"].as_str().unwrap_or(""));
                    }
                }
                None => println!("no frame selected (the task is not stopped)"),
            },
            "bt" | "where" => {
                self.frames = self.c.stack(self.task).unwrap_or_default();
                for (i, f) in self.frames.iter().enumerate() {
                    println!("{} #{i} {}", if i == self.frame { "*" } else { " " }, describe_frame(f));
                }
            }
            "up" | "down" => {
                if cmd == "up" && self.frame + 1 < self.frames.len() {
                    self.frame += 1;
                } else if cmd == "down" && self.frame > 0 {
                    self.frame -= 1;
                }
                if let Some(f) = self.frames.get(self.frame).cloned() {
                    println!("#{} {}", self.frame, describe_frame(&f));
                    self.show_line(&f);
                }
            }
            "tasks" | "threads" => match self.c.request("threads", json!({})) {
                Ok(r) => {
                    for t in r["threads"].as_array().cloned().unwrap_or_default() {
                        let id = t["id"].as_i64().unwrap_or(0);
                        let at = self.c.stack(id).ok().and_then(|f| f.first().map(describe_frame)).unwrap_or_else(|| "(running)".into());
                        println!("{} {:>3} {:<24} {}", if id == self.task { "*" } else { " " }, id, t["name"].as_str().unwrap_or(""), at);
                    }
                }
                Err(e) => println!("error: {e}"),
            },
            "task" | "thread" => {
                if let Ok(id) = rest.parse::<i64>() {
                    self.task = id;
                    self.frame = 0;
                    self.frames = self.c.stack(id).unwrap_or_default();
                    match self.frames.first().cloned() {
                        Some(f) => {
                            println!("task {id} at {}", describe_frame(&f));
                            self.show_line(&f);
                        }
                        None => println!("task {id} is running"),
                    }
                }
            }
            "catch" => {
                let mut parts = rest.split_whitespace();
                match parts.next() {
                    Some(k @ ("panic" | "error")) => {
                        if !self.exc.iter().any(|e| e == k) {
                            self.exc.push(k.to_string());
                        }
                        if k == "error" {
                            self.err_types = parts.collect::<Vec<_>>().join(",");
                        }
                    }
                    _ => {
                        println!("usage: catch panic | catch error [Type]");
                        return true;
                    }
                }
                let opts: Vec<Value> = self.exc.iter().map(|f| if f == "error" { json!({"filterId": f, "condition": self.err_types}) } else { json!({"filterId": f}) }).collect();
                if let Err(e) = self.c.request("setExceptionBreakpoints", json!({"filters": [], "filterOptions": opts})) {
                    println!("error: {e}");
                }
            }
            "pause" => {
                let _ = self.c.request("pause", json!({"threadId": self.task}));
            }
            "q" | "quit" | "exit" => return false,
            _ => println!("unknown command `{cmd}` (help: h)"),
        }
        true
    }
}

fn describe_frame(f: &Value) -> String {
    let path = f["source"]["path"].as_str().unwrap_or("?");
    let short = Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
    format!("{} ({}:{})", f["name"].as_str().unwrap_or("?"), short, f["line"])
}

/// The interactive debugger on connection `c`: sets up the session, then
/// reads commands from stdin until `q` or the end of the program.
/// `child` (a program started by `zell debug run`) is killed on `q`.
pub fn repl(c: Client, mut child: Option<Child>) -> i32 {
    let mut r = Repl { c, task: 1, frame: 0, frames: Vec::new(), file: String::new(), bps: Vec::new(), funcs: Vec::new(), exc: vec!["panic".into()], err_types: String::new(), done: false };
    if let Err(e) = r.c.request("initialize", json!({"clientID": "zell-cli", "adapterID": "zell", "linesStartAt1": true, "columnsStartAt1": true})) {
        eprintln!("error: {e}");
        return 1;
    }
    let _ = r.c.wait_event("initialized", Duration::from_secs(10));
    let _ = r.c.request("attach", json!({}));
    let _ = r.c.request("setExceptionBreakpoints", json!({"filters": ["panic"]}));
    println!("zell debug: set breakpoints, then `c` to start the program (help: h)");
    let stdin = std::io::stdin();
    let mut started = false;
    loop {
        r.print_events();
        if r.done {
            break;
        }
        print!("(zell) ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if !started && matches!(trimmed, "c" | "continue" | "n" | "next" | "s" | "step" | "r" | "run") {
            started = true;
            if matches!(trimmed, "n" | "next" | "s" | "step") {
                let _ = r.c.request("launch", json!({"stopOnEntry": true}));
            }
            let _ = r.c.request("configurationDone", json!({}));
            loop {
                match r.c.wait_any(&["stopped", "terminated", "output", "exited"], Duration::from_secs(3600)) {
                    Ok(e) => {
                        let ev = e["event"].as_str().unwrap_or("").to_string();
                        r.show_event(&e);
                        if ev == "stopped" || ev == "terminated" {
                            break;
                        }
                    }
                    Err(_) => {
                        r.done = true;
                        break;
                    }
                }
            }
            continue;
        }
        if !r.command(trimmed) {
            let _ = r.c.request("disconnect", json!({"terminateDebuggee": child.is_some()}));
            break;
        }
    }
    match child.as_mut() {
        Some(ch) => {
            let st = ch.wait().ok().and_then(|s| s.code()).unwrap_or(0);
            st
        }
        None => 0,
    }
}
