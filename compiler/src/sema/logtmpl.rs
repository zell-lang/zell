//! Compiled `log` message templates (docs/FRAMEWORK.md section 4).
//!
//! `logger.Info("user {UserId} bought {@Order} for {Amount:0.00}", id,
//! order, total)` on a `log.Logger` (or `*log.Pipeline`), and the
//! package functions `log.Info(...)` (the default logger), are not ordinary
//! calls. The template must be a string literal; it is parsed here, the
//! number of holes must equal the number of arguments (a compile error
//! otherwise), and the call is lowered to
//!
//! ```text
//! log.enabled(l, Level.Information) &&
//!     log.emit(l, Level.Information, err, text, parts, names, formats, values)
//! ```
//!
//! so the arguments are only evaluated when the level is enabled (`l` is
//! the receiver boxed as a `Logger`, or `nil` for the default logger). When
//! the receiver is not a plain place (a call, say) it is evaluated once and
//! the level is checked inside `emit` instead. `parts` are the literal
//! pieces around the holes (`len(names) + 1` of them, `{{`/`}}` unescaped),
//! `names` the property names, `formats` the text after the name
//! (`,10:0.00`); `values` are the arguments as `any`: `{@X}` through
//! `log.destructure<T>` (compile-time reflection, `@log(skip: true)`),
//! `{$X}` as their `println` text, others boxed directly (numbers, strings,
//! bools, errors, `any`) or through `log.capture<T>`.
//!
//! Every level method (`Verbose`, `Debug`, `Information`/`Info`,
//! `Warning`/`Warn`, `Error`, `Fatal`) also accepts an error first:
//! `logger.Error(err, "payment failed for {UserId}", id)`.

use super::coerce::Ctx;
use super::generics::{Inst, TmplDecl};
use super::*;
use crate::hir::{BinOp, Callee, ConstValue, ExprKind};

/// Level methods and the `log.Level` variant each one logs at.
const LEVELS: &[(&str, &str)] = &[
    ("Verbose", "Verbose"),
    ("Debug", "Debug"),
    ("Information", "Information"),
    ("Info", "Information"),
    ("Warning", "Warning"),
    ("Warn", "Warning"),
    ("Error", "Error"),
    ("Fatal", "Fatal"),
];

/// One hole of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hole {
    /// Property name (`UserId`, or `0` for positional holes).
    pub name: String,
    /// `'@'` destructure, `'$'` stringify, `' '` plain.
    pub op: char,
    /// What follows the name: `,10:0.00`, `:0.00` or empty.
    pub spec: String,
}

/// Splits a message template into the literal parts around its holes
/// (`holes.len() + 1` parts) and the holes. `{{` and `}}` are literal
/// braces; a lone `}` is literal; a `{` must start a valid hole
/// (`{Name}`, `{@Name}`, `{$Name}`, optionally `,align` and `:format`).
pub fn parse_template(t: &str) -> Result<(Vec<String>, Vec<Hole>), String> {
    let mut parts = Vec::new();
    let mut holes = Vec::new();
    let mut lit = String::new();
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' if b.get(i + 1) == Some(&b'{') => {
                lit.push('{');
                i += 2;
            }
            b'}' if b.get(i + 1) == Some(&b'}') => {
                lit.push('}');
                i += 2;
            }
            b'{' => {
                let Some(end) = t[i..].find('}') else {
                    return Err(format!("unclosed '{{' at byte {i} (write '{{{{' for a literal brace)"));
                };
                let body = &t[i + 1..i + end];
                let (op, rest) = match body.as_bytes().first() {
                    Some(b'@') => ('@', &body[1..]),
                    Some(b'$') => ('$', &body[1..]),
                    _ => (' ', body),
                };
                let name_end = rest.find([',', ':']).unwrap_or(rest.len());
                let name = &rest[..name_end];
                let spec = &rest[name_end..];
                let valid = !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') && (name.bytes().all(|c| c.is_ascii_digit()) || !name.as_bytes()[0].is_ascii_digit());
                if !valid {
                    return Err(format!("invalid placeholder {{{body}}} (expected {{Name}}, {{@Name}}, {{$Name}} or {{0}}, with optional ,align and :format)"));
                }
                if let Some(align) = spec.strip_prefix(',') {
                    let a = align.split(':').next().unwrap_or("");
                    if a.trim_start_matches('-').is_empty() || !a.trim_start_matches('-').bytes().all(|c| c.is_ascii_digit()) {
                        return Err(format!("invalid alignment in placeholder {{{body}}} (expected ,N or ,-N)"));
                    }
                }
                parts.push(std::mem::take(&mut lit));
                holes.push(Hole { name: name.to_string(), op, spec: spec.to_string() });
                i += end + 1;
            }
            _ => {
                let ch = t[i..].chars().next().unwrap();
                lit.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    parts.push(lit);
    Ok((parts, holes))
}

impl<'a> Sema<'a> {
    /// `log` namespace index, if registered.
    fn log_ns(&self) -> Option<NsIdx> {
        self.ns_index.get("log").copied()
    }

    /// Template id of a `log` declaration.
    fn log_template(&self, name: &str) -> Option<u32> {
        match self.nss[self.log_ns()?].members.get(name)? {
            (Member::Generic(t), _, _) => Some(*t),
            _ => None,
        }
    }

    /// The `log.Logger` interface type.
    fn log_logger_ty(&mut self) -> Option<TypeId> {
        match self.nss[self.log_ns()?].members.get("Logger")? {
            (Member::Interface(i), _, _) => {
                let i = *i;
                self.force_iface(i);
                Some(self.tt.intern(Type::Interface(crate::types::DeclId(i))))
            }
            _ => None,
        }
    }

    /// Whether values of `ty` get compiled level methods: `log.Logger`
    /// and `log.Pipeline` / `*log.Pipeline`.
    fn is_log_receiver(&mut self, ty: TypeId) -> bool {
        if let Some(sid) = self.struct_of(ty) {
            return self.structs[sid as usize].full_name == "log.Pipeline";
        }
        Some(ty) == self.log_logger_ty()
    }

    /// Hook of [`method_call_targs`](Self::method_call_targs): a level
    /// method on a logger. Gives `base` back if it is no such call.
    pub(crate) fn log_method_call(&mut self, base: hir::Expr, name: &ast::Ident, targs: Option<&[ast::TypeExpr]>, args: &[ast::Expr], span: Span) -> Result<hir::Expr, hir::Expr> {
        let Some(&(_, level)) = LEVELS.iter().find(|(m, _)| *m == name.name) else { return Err(base) };
        if targs.is_some() || !self.is_log_receiver(base.ty) {
            return Err(base);
        }
        let Some(lt) = self.log_logger_ty() else { return Err(base) };
        let pure = is_place(&base);
        let base = if matches!(self.tt.get(base.ty), Type::Struct(_)) { self.addr_of(base) } else { base };
        let logger = self.coerce(base, lt, Ctx::Other);
        Ok(self.log_lower(logger, pure, level, &format!("Logger.{}", name.name), args, span))
    }

    /// Hook of [`call_res`](Self::call_res): `log.Info(...)` and the other
    /// package-level level functions (default logger).
    pub(crate) fn log_func_call(&mut self, t: u32, args: &[ast::Expr], span: Span) -> Option<hir::Expr> {
        let tm = &self.gen.templates[t as usize];
        if !tm.is_std || !matches!(tm.decl, TmplDecl::Func(_)) || Some(tm.ns) != self.log_ns() {
            return None;
        }
        let &(m, level) = LEVELS.iter().find(|(m, _)| *m == tm.name)?;
        let lt = self.log_logger_ty()?;
        let nil = mk(ExprKind::Nil, lt, span);
        Some(self.log_lower(nil, true, level, &format!("log.{m}"), args, span))
    }

    /// A plain `log` function by name, as a callee.
    fn log_func(&mut self, name: &str, span: Span) -> Option<FuncId> {
        let t = self.log_template(name)?;
        match self.plain_std_inst(t, span)? {
            Inst::Func(f) => Some(f),
            _ => None,
        }
    }

    /// A generic `log` function instantiated for `ty`.
    fn log_generic(&mut self, name: &str, ty: TypeId, span: Span) -> Option<FuncId> {
        let t = self.log_template(name)?;
        match self.instantiate(t, vec![ty], span)? {
            Inst::Func(f) => Some(f),
            _ => None,
        }
    }

    /// The `log.Level` value `variant`.
    fn log_level(&mut self, variant: &str, span: Span) -> Option<hir::Expr> {
        let t = self.log_template("Level")?;
        let ty = self.plain_std_type(t, span)?;
        let Type::Enum(e) = *self.tt.get(ty) else { return None };
        let v = self.enums[e.0 as usize].variants.iter().position(|x| x.name == variant)? as u32;
        Some(mk(ExprKind::EnumLit { enum_id: e.0, variant: v, args: Vec::new() }, ty, span))
    }

    /// Lowers a level call (see the module docs).
    fn log_lower(&mut self, logger: hir::Expr, pure: bool, level: &str, what: &str, args: &[ast::Expr], span: Span) -> hir::Expr {
        // Optional leading error, then the template literal.
        let (err, rest) = match args.first().map(|a| &expr::strip_parens(a).kind) {
            Some(ast::ExprKind::Str(_)) => (None, args),
            Some(_) => {
                let e = self.expr_to(&args[0], self.tt.error, Ctx::Arg(0, what.to_string()));
                (Some(e), &args[1..])
            }
            None => {
                self.err(format!("{what} needs a message template"), span);
                return self.invalid(span);
            }
        };
        let text = match rest.first().map(|a| &expr::strip_parens(a).kind) {
            Some(ast::ExprKind::Str(s)) => s.clone(),
            _ => {
                let sp = rest.first().map_or(span, |a| a.span);
                self.err(format!("{what}: the message template must be a string literal (it is parsed at compile time)"), sp);
                self.check_args_loose(rest);
                return self.invalid(span);
            }
        };
        let tspan = rest[0].span;
        let vals = &rest[1..];
        let (parts, holes) = match parse_template(&text) {
            Ok(x) => x,
            Err(m) => {
                self.err(format!("{what}: {m}"), tspan);
                self.check_args_loose(vals);
                return self.invalid(span);
            }
        };
        if holes.len() != vals.len() {
            let list: Vec<String> = holes.iter().map(|h| format!("{{{}{}}}", if h.op == ' ' { String::new() } else { h.op.to_string() }, h.name)).collect();
            let shown = if list.is_empty() { String::new() } else { format!(" ({})", list.join(", ")) };
            self.err(
                format!(
                    "{what}: the template has {} placeholder{}{shown} but {} argument{} given",
                    holes.len(),
                    if holes.len() == 1 { "" } else { "s" },
                    vals.len(),
                    if vals.len() == 1 { " is" } else { "s are" }
                ),
                tspan,
            );
            self.check_args_loose(vals);
            return self.invalid(span);
        }
        let any = self.tt.any;
        let mut values = Vec::new();
        for (h, a) in holes.iter().zip(vals) {
            let v = self.value(a, None);
            let v = self.default_type(v);
            values.push(self.log_box(v, h.op));
        }
        let (string, bool_) = (self.tt.string, self.tt.bool_);
        let sl_str = self.tt.slice(string);
        let strs = |v: Vec<String>| mk(ExprKind::SliceLit(v.into_iter().map(|s| mk(ExprKind::Const(ConstValue::Str(s)), string, span)).collect()), sl_str, span);
        let names = strs(holes.iter().map(|h| h.name.clone()).collect());
        let formats = strs(holes.iter().map(|h| h.spec.clone()).collect());
        let parts = strs(parts);
        let sl_any = self.tt.slice(any);
        let values = mk(ExprKind::SliceLit(values), sl_any, span);
        let (Some(lvl), Some(emit), Some(enabled)) = (self.log_level(level, span), self.log_func("emit", span), self.log_func("enabled", span)) else {
            return self.invalid(span);
        };
        let err = err.unwrap_or_else(|| mk(ExprKind::Nil, self.tt.error, span));
        let text = mk(ExprKind::Const(ConstValue::Str(text)), string, tspan);
        let call = mk(ExprKind::Call { callee: Callee::Func(emit), args: vec![logger.clone(), lvl.clone(), err, text, parts, names, formats, values] }, bool_, span);
        if !pure {
            // emit checks the level itself; the receiver is evaluated once.
            return call;
        }
        let check = mk(ExprKind::Call { callee: Callee::Func(enabled), args: vec![logger, lvl] }, bool_, span);
        mk(ExprKind::Binary { op: BinOp::And, lhs: Box::new(check), rhs: Box::new(call) }, bool_, span)
    }

    /// A template argument as `any` (see the module docs).
    fn log_box(&mut self, v: hir::Expr, op: char) -> hir::Expr {
        let any = self.tt.any;
        let span = v.span;
        if self.tt.is_invalid(v.ty) {
            return v;
        }
        let direct = matches!(self.tt.get(v.ty), Type::Int(crate::types::IntKind::I64) | Type::Float64 | Type::String | Type::Bool | Type::Error | Type::Any);
        if op == '$' {
            let s = if v.ty == self.tt.string { v } else { mk(ExprKind::ToString(Box::new(v)), self.tt.string, span) };
            return self.coerce(s, any, Ctx::Other);
        }
        if direct && op != '@' {
            return self.coerce(v, any, Ctx::Other);
        }
        let name = if op == '@' { "destructure" } else { "capture" };
        let ty = v.ty;
        match self.log_generic(name, ty, span) {
            Some(f) => mk(ExprKind::Call { callee: Callee::Func(f), args: vec![v] }, any, span),
            None => self.invalid(span),
        }
    }
}

/// A receiver that can be evaluated twice: a local, upvalue, global or
/// `nil`, possibly through fields and dereferences.
fn is_place(e: &hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Local(_) | ExprKind::Upvalue(_) | ExprKind::Global(_) | ExprKind::Nil => true,
        ExprKind::Field { base, .. } | ExprKind::Deref(base) | ExprKind::ToInterface { value: base } => is_place(base),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_parts_and_holes() {
        let (p, h) = parse_template("user {UserId} bought {@Order} for {Amount:0.00}").unwrap();
        assert_eq!(p, vec!["user ", " bought ", " for ", ""]);
        assert_eq!(h.iter().map(|h| (h.name.as_str(), h.op, h.spec.as_str())).collect::<Vec<_>>(), vec![("UserId", ' ', ""), ("Order", '@', ""), ("Amount", ' ', ":0.00")]);
    }

    #[test]
    fn escapes_and_errors() {
        let (p, h) = parse_template("{{x}} {0} {$V,-5:x} }").unwrap();
        assert_eq!(p, vec!["{x} ", " ", " }"]);
        assert_eq!(h[1], Hole { name: "V".into(), op: '$', spec: ",-5:x".into() });
        assert!(parse_template("a {b").is_err());
        assert!(parse_template("{}").is_err());
        assert!(parse_template("{a b}").is_err());
        assert!(parse_template("{1x}").is_err());
        assert!(parse_template("{a,x}").is_err());
    }
}
