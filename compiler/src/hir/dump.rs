//! Readable text form of a [`Program`] (`zell check --dump-hir`) and the
//! shared type-name formatter.

use super::*;
use crate::types::Type;
use std::fmt::Write;

/// Names of nominal types, for [`type_name`].
pub trait DeclNames {
    fn struct_name(&self, id: u32) -> String;
    fn interface_name(&self, id: u32) -> String;
    fn enum_name(&self, id: u32) -> String;
    /// Name of generic type parameter `id` (only seen by the analyser).
    fn param_name(&self, id: u32) -> String {
        format!("T#{id}")
    }
}

impl DeclNames for Program {
    fn struct_name(&self, id: u32) -> String {
        self.structs[id as usize].full_name.clone()
    }
    fn interface_name(&self, id: u32) -> String {
        self.interfaces[id as usize].full_name.clone()
    }
    fn enum_name(&self, id: u32) -> String {
        self.enums[id as usize].full_name.clone()
    }
}

/// Source-like spelling of a type, e.g. `map[string]*app.models.User`.
pub fn type_name(tt: &TypeTable, id: TypeId, names: &dyn DeclNames) -> String {
    let n = |t: TypeId| type_name(tt, t, names);
    match tt.get(id) {
        Type::Pointer(t) => format!("*{}", n(*t)),
        Type::Weak(t) => format!("weak *{}", n(*t)),
        Type::Slice(t) => format!("[]{}", n(*t)),
        Type::Array(t, len) => format!("[{len}]{}", n(*t)),
        Type::Map(k, v) => format!("map[{}]{}", n(*k), n(*v)),
        Type::Tuple(ts) => format!("({})", ts.iter().map(|t| n(*t)).collect::<Vec<_>>().join(", ")),
        Type::Fallible(t) => format!("{}!", n(*t)),
        Type::Task(t) => format!("Task<{}>", n(*t)),
        Type::Func(sig) => {
            let ps = sig.params.iter().map(|t| n(*t)).collect::<Vec<_>>().join(", ");
            if matches!(tt.get(sig.ret), Type::Void) {
                format!("func({ps})")
            } else {
                format!("func({ps}) {}", n(sig.ret))
            }
        }
        Type::Struct(d) => names.struct_name(d.0),
        Type::Interface(d) => names.interface_name(d.0),
        Type::Enum(d) => names.enum_name(d.0),
        Type::Param(p) => names.param_name(*p),
        _ => tt.display(id, &|d| format!("decl#{}", d.0)),
    }
}

impl Program {
    /// Spelling of a type using this program's declarations.
    pub fn type_name(&self, ty: TypeId) -> String {
        type_name(&self.types, ty, self)
    }

    /// The whole program as indented text.
    pub fn dump(&self) -> String {
        let plain = super::suspend::plain_async(self).plain;
        let mut d = Dumper { p: self, out: String::new(), indent: 0, func: None, plain };
        d.program();
        d.out
    }
}

struct Dumper<'a> {
    p: &'a Program,
    out: String,
    indent: usize,
    func: Option<&'a Function>,
    /// [`super::suspend::plain_async`]: marked on the function line.
    plain: Vec<bool>,
}

impl<'a> Dumper<'a> {
    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn ty(&self, t: TypeId) -> String {
        self.p.type_name(t)
    }

    fn attrs(&self, attrs: &[Attr]) -> String {
        let mut s = String::new();
        for a in attrs {
            let name = match &a.kind {
                AttrKind::Builtin(n) => n.clone(),
                AttrKind::User(i) => self.p.structs[*i as usize].full_name.clone(),
            };
            let args: Vec<String> = a.args.iter().map(|(k, v)| format!("{k}: {}", const_str(v))).collect();
            let _ = write!(s, "@{name}({}) ", args.join(", "));
        }
        s
    }

    fn program(&mut self) {
        let p = self.p;
        for ns in &p.namespaces {
            let line = format!("namespace {} {}", ns.path, self.attrs(&ns.attrs));
            self.line(line.trim_end());
        }
        for (i, s) in p.structs.iter().enumerate() {
            let line = format!("{}struct #{i} {}", self.attrs(&s.attrs), s.full_name);
            self.line(&line);
            self.indent += 1;
            for (fi, f) in s.fields.iter().enumerate() {
                let emb = if f.embedded { " (embedded)" } else { "" };
                let def = f.default.as_ref().map(|d| format!(" = {}", const_str(d))).unwrap_or_default();
                let line = format!("{}field {fi} {} {}{def}{emb}", self.attrs(&f.attrs), f.name, self.ty(f.ty));
                self.line(&line);
            }
            for (name, fid) in &s.methods {
                self.line(&format!("method {name} -> func #{}", fid.0));
            }
            self.indent -= 1;
        }
        for (i, it) in p.interfaces.iter().enumerate() {
            self.line(&format!("interface #{i} {}", it.full_name));
            self.indent += 1;
            for (mi, m) in it.methods.iter().enumerate() {
                let ps: Vec<String> = m.params.iter().map(|t| self.ty(*t)).collect();
                let line = format!("method {mi} {}({}) {}", m.name, ps.join(", "), self.ty(m.ret));
                self.line(&line);
            }
            self.indent -= 1;
        }
        for (i, e) in p.enums.iter().enumerate() {
            self.line(&format!("enum #{i} {}{}", e.full_name, if e.is_plain { " (plain)" } else { "" }));
            self.indent += 1;
            for (vi, v) in e.variants.iter().enumerate() {
                let fs: Vec<String> = v.fields.iter().map(|(n, t)| format!("{n} {}", self.ty(*t))).collect();
                self.line(&format!("variant {vi} {}({})", v.name, fs.join(", ")));
            }
            self.indent -= 1;
        }
        for (i, x) in p.externs.iter().enumerate() {
            let ps: Vec<String> = x.params.iter().map(|t| self.ty(*t)).collect();
            let line = format!("extern #{i} {} = {}({}) {}", x.path, x.symbol, ps.join(", "), self.ty(x.ret));
            self.line(&line);
        }
        for (i, g) in p.globals.iter().enumerate() {
            let kw = if g.is_const { "const" } else { "var" };
            let mut line = format!("{kw} #{i} {} {}", g.full_name, self.ty(g.ty));
            if let Some(v) = &g.value {
                let _ = write!(line, " = {}", const_str(v));
            }
            if let Some(e) = &g.init {
                let _ = write!(line, " = {}", self.expr(e));
            }
            self.line(&line);
        }
        for (i, f) in p.funcs.iter().enumerate() {
            self.function(i, f);
        }
        if let Some(e) = p.entry {
            self.line(&format!("entry func #{}", e.0));
        }
    }

    fn function(&mut self, i: usize, f: &'a Function) {
        self.func = Some(f);
        let params: Vec<String> = f.params.iter().map(|l| self.local(*l)).collect();
        // A never-suspending async function codegen lowers as a plain one.
        let asy = match (f.is_async, self.plain.get(i).copied().unwrap_or(false)) {
            (true, true) => "async(plain) ",
            (true, false) => "async ",
            _ => "",
        };
        let line = format!(
            "{}{asy}func #{i} {} [{}] {:?} ({}) {}",
            self.attrs(&f.attrs),
            f.full_name,
            f.symbol,
            f.kind,
            params.join(", "),
            self.ty(f.ret)
        );
        self.line(&line);
        self.indent += 1;
        for (ci, c) in f.captures.iter().enumerate() {
            self.line(&format!("capture {ci} {} {} from {:?}", c.name, self.ty(c.ty), c.source));
        }
        for (li, l) in f.locals.iter().enumerate() {
            let mut flags = String::new();
            if l.captured {
                flags.push_str(" captured");
            }
            if l.address_taken {
                flags.push_str(" addr");
            }
            self.line(&format!("local %{li} {} {} {:?}{flags}", l.name, self.ty(l.ty), l.kind));
        }
        self.block(&f.body);
        self.indent -= 1;
    }

    fn local(&self, l: LocalId) -> String {
        match self.func {
            Some(f) => format!("%{}:{}", l.0, f.locals[l.0 as usize].name),
            None => format!("%{}", l.0),
        }
    }

    fn opt_local(&self, l: Option<LocalId>) -> String {
        l.map(|l| self.local(l)).unwrap_or_else(|| "_".into())
    }

    fn block(&mut self, b: &Block) {
        self.line("{");
        self.indent += 1;
        for s in &b.stmts {
            self.stmt(s);
        }
        self.indent -= 1;
        self.line("}");
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { local, init } => {
                let init = init.as_ref().map(|e| self.expr(e)).unwrap_or_else(|| "zero".into());
                let line = format!("let {} = {init}", self.local(*local));
                self.line(&line);
            }
            StmtKind::LetTuple { locals, init } => {
                let ls: Vec<String> = locals.iter().map(|l| self.opt_local(*l)).collect();
                let line = format!("let ({}) = {}", ls.join(", "), self.expr(init));
                self.line(&line);
            }
            StmtKind::Assign { target, value } => {
                let line = format!("{} = {}", self.expr(target), self.expr(value));
                self.line(&line);
            }
            StmtKind::CompoundAssign { target, op, value } => {
                let line = format!("{} {op:?}= {}", self.expr(target), self.expr(value));
                self.line(&line);
            }
            StmtKind::Expr(e) => {
                let line = self.expr(e);
                self.line(&line);
            }
            StmtKind::Discard(e) => {
                let line = format!("_ = {}", self.expr(e));
                self.line(&line);
            }
            StmtKind::If { cond, then, else_ } => {
                let line = format!("if {}", self.expr(cond));
                self.line(&line);
                self.block(then);
                if let Some(e) = else_ {
                    self.line("else");
                    self.block(e);
                }
            }
            StmtKind::Loop { label, cond, body } => {
                let c = cond.as_ref().map(|c| self.expr(c)).unwrap_or_default();
                self.line(&format!("loop L{} {c}", label.0));
                self.block(body);
            }
            StmtKind::ForRange { label, var, start, end, body } => {
                let line = format!("for L{} {} in {}..{}", label.0, self.opt_local(*var), self.expr(start), self.expr(end));
                self.line(&line);
                self.block(body);
            }
            StmtKind::ForSlice { label, index, elem, seq, body } => {
                let line = format!("for-slice L{} {}, {} in {}", label.0, self.opt_local(*index), self.opt_local(*elem), self.expr(seq));
                self.line(&line);
                self.block(body);
            }
            StmtKind::ForString { label, index, ch, string, body } => {
                let line = format!("for-string L{} {}, {} in {}", label.0, self.opt_local(*index), self.opt_local(*ch), self.expr(string));
                self.line(&line);
                self.block(body);
            }
            StmtKind::ForMap { label, key, value, map, body } => {
                let line = format!("for-map L{} {}, {} in {}", label.0, self.opt_local(*key), self.opt_local(*value), self.expr(map));
                self.line(&line);
                self.block(body);
            }
            StmtKind::Switch(sw) => {
                let subj = sw.subject.as_ref().map(|(l, e)| format!("{} = {}", self.local(*l), self.expr(e))).unwrap_or_default();
                self.line(&format!("switch L{} {subj}", sw.label.0));
                self.indent += 1;
                for c in &sw.cases {
                    let ps: Vec<String> = c.patterns.iter().map(|p| self.pattern(p)).collect();
                    self.line(&format!("case {}", ps.join(" | ")));
                    self.block(&c.body);
                }
                if let Some(d) = &sw.default {
                    self.line("default");
                    self.block(d);
                }
                self.indent -= 1;
            }
            StmtKind::Return(r) => {
                let line = format!("return {}", self.ret(r));
                self.line(line.trim_end());
            }
            StmtKind::Break(l) => self.line(&format!("break L{}", l.0)),
            StmtKind::Continue(l) => self.line(&format!("continue L{}", l.0)),
            StmtKind::Defer(e) => {
                let line = format!("defer {}", self.expr(e));
                self.line(&line);
            }
            StmtKind::Block(b) => self.block(b),
            StmtKind::TryBlock { body, err, handler } => {
                self.line("try");
                self.block(body);
                let line = format!("catch {}", self.opt_local(*err));
                self.line(&line);
                self.block(handler);
            }
        }
    }

    fn ret(&self, r: &ReturnValue) -> String {
        match r {
            ReturnValue::Void => String::new(),
            ReturnValue::Value(e) => self.expr(e),
            ReturnValue::Error(e) => format!("error {}", self.expr(e)),
        }
    }

    fn pattern(&self, p: &Pattern) -> String {
        match p {
            Pattern::Cond(e) => self.expr(e),
            Pattern::Variant { variant, bindings } => {
                let bs: Vec<String> = bindings.iter().map(|b| self.opt_local(*b)).collect();
                format!("variant {variant}({})", bs.join(", "))
            }
            Pattern::Type { ty, binding } => format!("type {} {}", self.ty(*ty), self.opt_local(*binding)),
        }
    }

    fn exprs(&self, es: &[Expr]) -> String {
        es.iter().map(|e| self.expr(e)).collect::<Vec<_>>().join(", ")
    }

    /// One-line rendering of an expression; nested blocks (catch handlers)
    /// are rendered inline with `;` separators.
    fn expr(&self, e: &Expr) -> String {
        let ty = self.ty(e.ty);
        let body = match &e.kind {
            ExprKind::Const(v) => const_str(v),
            ExprKind::Nil => "nil".into(),
            ExprKind::Zero => "zero".into(),
            ExprKind::AttrConst { attr, .. } => format!("attr {:?}", attr.kind),
            ExprKind::Local(l) => self.local(*l),
            ExprKind::Upvalue(i) => {
                let name = self.func.and_then(|f| f.captures.get(*i as usize)).map(|c| c.name.clone()).unwrap_or_default();
                format!("^{i}:{name}")
            }
            ExprKind::Global(g) => format!("global {}", self.p.globals[g.0 as usize].full_name),
            ExprKind::FuncRef(f) => format!("funcref {}", self.p.funcs[f.0 as usize].full_name),
            ExprKind::Field { base, index } => {
                let name = match self.p.types.get(base.ty) {
                    Type::Struct(d) => self.p.structs[d.0 as usize].fields[*index as usize].name.clone(),
                    _ => index.to_string(),
                };
                format!("{}.{name}", self.expr(base))
            }
            ExprKind::Deref(x) => format!("deref({})", self.expr(x)),
            ExprKind::AddrOf(x) => format!("addr({})", self.expr(x)),
            ExprKind::New(x) => format!("new({})", self.expr(x)),
            ExprKind::Index { base, index } => format!("{}[{}]", self.expr(base), self.expr(index)),
            ExprKind::MapGet { map, key } => format!("mapget({}, {})", self.expr(map), self.expr(key)),
            ExprKind::SubSlice { base, lo, hi } => {
                let lo = lo.as_ref().map(|x| self.expr(x)).unwrap_or_default();
                let hi = hi.as_ref().map(|x| self.expr(x)).unwrap_or_default();
                format!("{}[{lo}..{hi}]", self.expr(base))
            }
            ExprKind::Unary { op, operand } => format!("{op:?}({})", self.expr(operand)),
            ExprKind::Binary { op, lhs, rhs } => format!("{op:?}({}, {})", self.expr(lhs), self.expr(rhs)),
            ExprKind::IsNil { value, negated } => format!("{}({})", if *negated { "notnil" } else { "isnil" }, self.expr(value)),
            ExprKind::Convert { kind, value } => format!("{kind:?}({})", self.expr(value)),
            ExprKind::Call { callee, args } => {
                let c = match callee {
                    Callee::Func(f) => self.p.funcs[f.0 as usize].full_name.clone(),
                    Callee::Extern(x) => format!("extern {}", self.p.externs[x.0 as usize].symbol),
                    Callee::Interface { recv, method } => format!("dyn {}.m{method}", self.expr(recv)),
                    Callee::Value(v) => format!("({})", self.expr(v)),
                };
                format!("call {c}({})", self.exprs(args))
            }
            ExprKind::StructLit { struct_id, fields } => {
                let s = &self.p.structs[*struct_id as usize];
                let fs: Vec<String> = fields.iter().map(|(i, v)| format!("{}: {}", s.fields[*i as usize].name, self.expr(v))).collect();
                format!("{}{{{}}}", s.name, fs.join(", "))
            }
            ExprKind::SliceLit(es) => format!("[{}]", self.exprs(es)),
            ExprKind::MapLit(es) => {
                let fs: Vec<String> = es.iter().map(|(k, v)| format!("{}: {}", self.expr(k), self.expr(v))).collect();
                format!("map{{{}}}", fs.join(", "))
            }
            ExprKind::Tuple(es) => format!("tuple({})", self.exprs(es)),
            ExprKind::EnumLit { enum_id, variant, args } => {
                let en = &self.p.enums[*enum_id as usize];
                format!("{}.{}({})", en.name, en.variants[*variant as usize].name, self.exprs(args))
            }
            ExprKind::ToInterface { value } => format!("to_iface({})", self.expr(value)),
            ExprKind::Closure { func, captures } => format!("closure func #{} {captures:?}", func.0),
            ExprKind::Propagate { value, context } => {
                let c = context.as_ref().map(|c| format!(" ctx {}", self.expr(c))).unwrap_or_default();
                format!("propagate({}{c})", self.expr(value))
            }
            ExprKind::TryCatch { value, err, handler, result_used } => {
                let mut sub = Dumper { p: self.p, out: String::new(), indent: 0, func: self.func, plain: Vec::new() };
                for s in &handler.stmts {
                    sub.stmt(s);
                }
                let h = sub.out.trim_end().replace('\n', "; ");
                format!("try {} catch {} {{ {h} }}{}", self.expr(value), self.opt_local(*err), if *result_used { "" } else { " (stmt)" })
            }
            ExprKind::TryElse { value, fallback } => {
                let f = match &**fallback {
                    TryFallback::Value(v) => self.expr(v),
                    TryFallback::Break(l) => format!("break L{}", l.0),
                    TryFallback::Continue(l) => format!("continue L{}", l.0),
                    TryFallback::Return(r) => format!("return {}", self.ret(r)),
                };
                format!("try {} else {f}", self.expr(value))
            }
            ExprKind::Await(x) => format!("await {}", self.expr(x)),
            ExprKind::TaskAll(es) => format!("task_all({})", self.exprs(es)),
            ExprKind::Builtin { op, args } => format!("{op:?}({})", self.exprs(args)),
            ExprKind::ToString(x) => format!("tostring({})", self.expr(x)),
            ExprKind::Print(items) => {
                let is: Vec<String> = items
                    .iter()
                    .map(|i| match &i.arg {
                        Some(a) => format!("{:?} {}", i.op, self.expr(a)),
                        None => format!("{:?}", i.op),
                    })
                    .collect();
                format!("print[{}]", is.join("; "))
            }
        };
        format!("{body}:{ty}")
    }
}

/// Source-like spelling of a constant.
pub fn const_str(v: &ConstValue) -> String {
    match v {
        ConstValue::Int(i) => i.to_string(),
        ConstValue::Float(f) => format!("{f:?}"),
        ConstValue::Bool(b) => b.to_string(),
        ConstValue::Char(c) => format!("{c:?}"),
        ConstValue::Str(s) => format!("{s:?}"),
        ConstValue::Enum(e, v) => format!("enum#{e}.{v}"),
        ConstValue::List(vs) => format!("[{}]", vs.iter().map(const_str).collect::<Vec<_>>().join(", ")),
        ConstValue::Name(n) => n.clone(),
    }
}
