//! Member completion after `.` typed by sema: the receiver is any
//! expression (calls, indexing, `await`, parentheses, lambdas), not only a
//! chain of names.

use super::*;

const SRC: &str = "namespace app

import net

interface Repo {
    func Find(id int) *User
    func Count() int
}

struct User {
    Name string
    Age  int

    func Greet() string {
        return this.Name
    }
}

struct Holder {
    func Get() *User {
        return &User{Name: \"a\", Age: 1}
    }
}

struct Builder {
    n int

    func Add(s string) *Builder {
        this.n = this.n + len(s)
        return this
    }

    func Build() string {
        return \"\"
    }
}

struct Service {
    repo Repo

    func Run() {
        var n = this.repo.Count()
        _ = n
    }
}

enum Color {
    Red
    Green

    func Hex() string {
        return \"\"
    }
}

func makeUser() *User {
    return &User{Name: \"b\", Age: 2}
}

func pair() (int, string) {
    return (1, \"x\")
}

async func load() *User {
    return makeUser()
}

async func work(h *Holder, r *http.Request) {
    var u = await load()
    _ = u
    var xs = [makeUser()]
    var m = map[string]*User{\"k\": makeUser()}
    var c = Color.Red
    var t = pair()
    var err = error(\"boom\")
    var f = (x *Holder) => x.Get().Age
    var b = &Builder{}
    _ = xs
    _ = m
    _ = c
    _ = t
    _ = err
    _ = f
    _ = b
    _ = h
    _ = r
    // here
}

func main() {
}
";

fn labels_after(c: &mut Client, uri: &Url, version: i32, anchor: &str, line: &str) -> Vec<String> {
    let edited = SRC.replacen(anchor, &format!("{anchor}\n{line}"), 1);
    c.change(uri, version, &edited);
    let r: Option<CompletionResponse> = c.request::<request::Completion>(CompletionParams {
        text_document_position: tdp(uri, pos_of(&edited, line, 0, line.len())),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    });
    let items = match r {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(l)) => l.items,
        None => Vec::new(),
    };
    items.into_iter().map(|i| i.label).collect()
}

#[test]
fn member_completion_on_typed_expressions() {
    let (dir, uri) = project_with("main.zl", SRC);
    let mut c = Client::start(&dir);
    c.open(&uri, SRC);
    let errors: Vec<_> = c.diagnostics(&uri).into_iter().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR)).collect();
    assert!(errors.is_empty(), "{errors:?}");
    let here = "    // here";
    let has = |labels: &[String], want: &[&str]| want.iter().all(|w| labels.iter().any(|l| l == w));
    let user = ["Name", "Age", "Greet"];
    let cases: &[(&str, &str, &[&str])] = &[
        // call result
        (here, "    makeUser().", &user),
        // method chain through a field
        (here, "    h.Get().", &user),
        (here, "    h.Get().Name.", &[]),
        // fluent chain on a builder returning *Builder
        (here, "    b.Add(\"x\").Add(\"y\").", &["Add", "Build"]),
        // a chain continued on the next line
        (here, "    b.Add(\"x\")\n        .", &["Add", "Build"]),
        // indexing and map values
        (here, "    xs[0].", &user),
        (here, "    m[\"k\"].", &user),
        // await
        (here, "    (await load()).", &user),
        // `await x.M().` awaits the whole chain: the receiver is the task.
        (here, "    await load().", &["Cancel"]),
        // std struct field chain
        (here, "    r.Reader.", &["Read", "ReadAll"]),
        // error value, enum value, tuple
        (here, "    err.", &["Message"]),
        (here, "    c.", &["Hex"]),
        (here, "    t.", &["0", "1"]),
        // partial prefix
        (here, "    h.Get().Na", &["Name"]),
        // inside a lambda body
        (here, "    var g = (x *Holder) => x.Get().", &user),
        // interface-typed field of `this`
        ("        _ = n", "        this.repo.", &["Find", "Count"]),
        ("        _ = n", "        this.repo.Find(1).", &user),
    ];
    for (i, (anchor, line, want)) in cases.iter().enumerate() {
        let labels = labels_after(&mut c, &uri, 2 + i as i32, anchor, line);
        if want.is_empty() {
            // `string` has no members.
            assert!(labels.is_empty(), "{line}: {labels:?}");
        } else {
            assert!(has(&labels, want), "{line}: {labels:?}");
        }
    }
    // Namespace completion still works.
    let labels = labels_after(&mut c, &uri, 100, here, "    net.http.");
    assert!(has(&labels, &["Request"]), "{labels:?}");
}
