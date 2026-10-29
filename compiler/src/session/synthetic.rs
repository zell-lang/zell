//! A generated workspace for timing the incremental analysis
//! (`compiler/tests/incremental_bench.rs`, the LSP bench in
//! `lsp/tests/server/bench.rs`; docs/TOOLING.md, "Performance").

/// The relative path and text of file `j` of namespace `app.m{k}`: about
/// 230 lines, a struct with methods and 13 functions; namespace `k > 0`
/// imports `app.m0` (a generic `Max`, a `TypesWith` attribute `Task`) and
/// `app.m{k-1}`, so the namespaces form a chain.
pub fn synthetic_file(k: usize, j: usize) -> (String, String) {
    let mut s = format!("namespace app.m{k}\n\n");
    if k > 0 {
        s += "import app.m0\n";
        s += &format!("import app.m{}\n", k - 1);
    }
    s += "\n";
    if k == 0 && j == 0 {
        s += "@attribute(targets: [reflect.Target.Struct])\nstruct Task {\n    name string\n}\n\n";
        s += "func Max<T: Ordered>(a, b T) T {\n    if a > b {\n        return a\n    }\n    return b\n}\n\n";
        s += "func Jobs() int {\n    return len(reflect.TypesWith<Task>())\n}\n\n";
    }
    if j == 0 {
        s += &format!("@Task(name: \"j{k}\")\nstruct Job{k} {{\n    static func Run() string {{\n        return \"j{k}\"\n    }}\n}}\n\n");
    }
    let t = format!("T{k}_{j}");
    s += &format!(
        "struct {t} {{\n    Id    int\n    Name  string\n    Score float\n\n    func Describe() string {{\n        return this.Name + \":\" + fmt.Format(\"{{0}}\", this.Id)\n    }}\n\n    func Bump(n int) int {{\n        this.Id += n\n        return this.Id\n    }}\n\n    static func New(id int, name string) {t} {{\n        return {t}{{Id: id, Name: name}}\n    }}\n}}\n\n"
    );
    for f in 0..13 {
        s += &format!("func Work{j}_{f}(a int, b int) int {{\n    var s = 0\n    for i in 0..a {{\n        if i % 3 == 0 {{\n            s += i * b\n        }} else {{\n            s -= 1\n        }}\n    }}\n    var t = {t}.New(a, \"x\")\n    s += t.Bump(b)\n");
        if k > 0 {
            s += &format!("    s += Max(a, b)\n    s += T{}_{j}.New(a, \"y\").Bump({f})\n", k - 1);
        }
        s += "    return s\n}\n\n";
    }
    (format!("m{k}/f{j}.zl"), s)
}
