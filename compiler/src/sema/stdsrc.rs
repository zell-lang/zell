//! Parts of the standard library written in Zell: `task` (`Channel<T>`,
//! `Mutex`, `WaitGroup`, `Spawn`, `Timeout`, `Context`), `net` (TCP
//! over non-blocking sockets), `net.http` (HTTP/1.1 client and
//! server), `log` (structured logging, with compiled message templates
//! in [`super::logtmpl`]), `diag` (metrics, tracing, profiles),
//! `crypto.x509` (certificates) and `net.tls` (TLS 1.3/1.2 in three
//! files: `tls.zl`, `tls_client.zl`, `tls_server.zl` share the namespace), and
//! the tier 0/1 library of docs/STDLIB.md (`slices`, `maps`, `cmp`,
//! `bytes`, `strconv`, `time` types, `math.bits/big/complex`,
//! `unicode.utf8/utf16`, `hash.*`, `encoding.binary/base32/pem/asn1`, `io`,
//! `os` files, `os.signal`, `os.user`, `process`, more `collections`, `sync`,
//! `cli`, `testing`, `embed`), and the tier 3 modules (`database.sql`
//! with the `postgres` and `sqlite` drivers, `net.mail`,
//! `crypto.kdf/password`, RSA key generation), and the tier 3b
//! infrastructure clients (`data.redis`, `data.kafka`, `cloud.s3`,
//! `cloud.blob`; C helpers in `runtime/src/std_infra.c`). The sources live in `sema/stdlib_src/*.zl`; like
//! `collections` ([`super::collections`]) they are parsed once per
//! process and registered as stdlib templates, so only what a program uses
//! is instantiated, checked and compiled.
//!
//! Rules for the sources (they are instantiated in the scope of the user
//! file that first uses them):
//!
//! * structs, functions and enums become templates, also without type
//!   parameters ("plain" templates, instantiated with no arguments on first
//!   use: `task.Mutex`, `net.Dial`); interfaces are registered as ordinary
//!   interfaces. Top-level `var`/`const` are not supported;
//! * names of the module's own namespace are written short (`Conn`); every
//!   other namespace is written with its full path (`strings.Index`,
//!   `net.Conn`), because the importing file's imports are in effect;
//!   interface method signatures always use full paths;
//! * the runtime is reached through the stdlib externs of
//!   [`super::stdlib::FUNCS`] whose names start with `rt` (internal).

use super::generics::{Inst, TmplDecl};
use super::*;
use std::sync::OnceLock;

/// The Zell-source modules: (namespace, file name, source).
pub(crate) const MODULES: &[(&str, &str, &str)] = &[
    ("task", "task.zl", include_str!("stdlib_src/task.zl")),
    ("net", "net.zl", include_str!("stdlib_src/net.zl")),
    ("net", "net_udp.zl", include_str!("stdlib_src/net_udp.zl")),
    ("net.http", "http.zl", include_str!("stdlib_src/http.zl")),
    ("net.http", "http_server.zl", include_str!("stdlib_src/http_server.zl")),
    ("net.http", "http_client.zl", include_str!("stdlib_src/http_client.zl")),
    ("net.http", "http2.zl", include_str!("stdlib_src/http2.zl")),
    ("net.http", "http_extra.zl", include_str!("stdlib_src/http_extra.zl")),
    ("net.http.hpack", "hpack.zl", include_str!("stdlib_src/hpack.zl")),
    ("net.http.test", "httptest.zl", include_str!("stdlib_src/httptest.zl")),
    ("net.ws", "ws.zl", include_str!("stdlib_src/ws.zl")),
    ("crypto.x509", "x509.zl", include_str!("stdlib_src/x509.zl")),
    ("net.tls", "tls.zl", include_str!("stdlib_src/tls.zl")),
    ("net.tls", "tls_client.zl", include_str!("stdlib_src/tls_client.zl")),
    ("net.tls", "tls_server.zl", include_str!("stdlib_src/tls_server.zl")),
    ("log", "log.zl", include_str!("stdlib_src/log.zl")),
    ("diag", "diag.zl", include_str!("stdlib_src/diag.zl")),
    ("diag", "diag_heap.zl", include_str!("stdlib_src/diag_heap.zl")),
    ("arena", "arena.zl", include_str!("stdlib_src/arena.zl")),
    ("runtime", "runtime_cycles.zl", include_str!("stdlib_src/runtime_cycles.zl")),
    // >>> debugger (docs/DEBUGGER.md stage 2)
    ("debug", "debug.zl", include_str!("stdlib_src/debug.zl")),
    ("debug", "debug_dap.zl", include_str!("stdlib_src/debug_dap.zl")),
    ("debug", "debug_vars.zl", include_str!("stdlib_src/debug_vars.zl")),
    // <<< debugger
    // >>> tier 2 stdlib (docs/STDLIB.md level 2)
    ("regex", "regex.zl", include_str!("stdlib_src/regex.zl")),
    ("regex", "regex_syntax.zl", include_str!("stdlib_src/regex_syntax.zl")),
    ("regex", "regex_vm.zl", include_str!("stdlib_src/regex_vm.zl")),
    ("compress", "compress.zl", include_str!("stdlib_src/compress.zl")),
    ("compress.deflate", "deflate.zl", include_str!("stdlib_src/deflate.zl")),
    ("compress.zlib", "zlib.zl", include_str!("stdlib_src/zlib.zl")),
    ("compress.gzip", "gzip.zl", include_str!("stdlib_src/gzip.zl")),
    ("compress.zstd", "zstd.zl", include_str!("stdlib_src/zstd.zl")),
    ("archive.zip", "zip.zl", include_str!("stdlib_src/zip.zl")),
    ("archive.tar", "tar.zl", include_str!("stdlib_src/tar.zl")),
    ("encoding.csv", "csv.zl", include_str!("stdlib_src/csv.zl")),
    ("encoding", "codec.zl", include_str!("stdlib_src/codec.zl")),
    ("encoding.xml", "xml.zl", include_str!("stdlib_src/xml.zl")),
    ("encoding.yaml", "yaml.zl", include_str!("stdlib_src/yaml.zl")),
    ("encoding.toml", "toml.zl", include_str!("stdlib_src/toml.zl")),
    ("text.template", "template.zl", include_str!("stdlib_src/template.zl")),
    ("text.template", "template_exec.zl", include_str!("stdlib_src/template_exec.zl")),
    ("text.template", "template_check.zl", include_str!("stdlib_src/template_check.zl")),
    ("text.template", "htmlctx.zl", include_str!("stdlib_src/htmlctx.zl")),
    ("html.template", "htmltemplate.zl", include_str!("stdlib_src/htmltemplate.zl")),
    ("net.textproto", "textproto.zl", include_str!("stdlib_src/textproto.zl")),
    ("mime", "mime.zl", include_str!("stdlib_src/mime.zl")),
    ("mime.quotedprintable", "quotedprintable.zl", include_str!("stdlib_src/quotedprintable.zl")),
    ("mime.multipart", "multipart.zl", include_str!("stdlib_src/multipart.zl")),
    ("net.url", "url.zl", include_str!("stdlib_src/url.zl")),
    ("text.scanner", "scanner.zl", include_str!("stdlib_src/scanner.zl")),
    ("text.table", "table.zl", include_str!("stdlib_src/table.zl")),
    // <<< tier 2 stdlib
    // >>> tier 3 stdlib (docs/STDLIB.md level 3)
    ("database.sql", "sql.zl", include_str!("stdlib_src/sql.zl")),
    ("database.sqlite", "sqlite.zl", include_str!("stdlib_src/sqlite.zl")),
    ("crypto.kdf", "kdf.zl", include_str!("stdlib_src/kdf.zl")),
    ("crypto.password", "password.zl", include_str!("stdlib_src/password.zl")),
    ("crypto.rsa", "rsa.zl", include_str!("stdlib_src/rsa.zl")),
    ("database.postgres", "postgres.zl", include_str!("stdlib_src/postgres.zl")),
    ("database.postgres", "postgres_types.zl", include_str!("stdlib_src/postgres_types.zl")),
    ("net.mail", "mail.zl", include_str!("stdlib_src/mail.zl")),
    ("compress.brotli", "brotli.zl", include_str!("stdlib_src/brotli.zl")),
    // <<< tier 3 stdlib
    // >>> tier 3b stdlib (docs/STDLIB.md level 3b: infrastructure clients)
    ("data.redis", "redis.zl", include_str!("stdlib_src/redis.zl")),
    ("data.redis", "redis_route.zl", include_str!("stdlib_src/redis_route.zl")),
    ("data.redis", "redis_client.zl", include_str!("stdlib_src/redis_client.zl")),
    ("data.kafka", "kafka.zl", include_str!("stdlib_src/kafka.zl")),
    ("data.kafka", "kafka_producer.zl", include_str!("stdlib_src/kafka_producer.zl")),
    ("data.kafka", "kafka_consumer.zl", include_str!("stdlib_src/kafka_consumer.zl")),
    ("data.kafka", "kafka_admin.zl", include_str!("stdlib_src/kafka_admin.zl")),
    ("cloud.s3", "s3.zl", include_str!("stdlib_src/s3.zl")),
    ("cloud.s3", "s3_multi.zl", include_str!("stdlib_src/s3_multi.zl")),
    ("cloud.blob", "blob.zl", include_str!("stdlib_src/blob.zl")),
    // <<< tier 3b stdlib
    ("encoding.json", "json_schema.zl", include_str!("stdlib_src/json_schema.zl")),
    // >>> data (docs/FRAMEWORK.md section 3)
    ("data", "data.zl", include_str!("stdlib_src/data.zl")),
    ("data", "data_query.zl", include_str!("stdlib_src/data_query.zl")),
    ("data", "data_save.zl", include_str!("stdlib_src/data_save.zl")),
    ("data", "data_migrate.zl", include_str!("stdlib_src/data_migrate.zl")),
    // <<< data
    // tier 0/1 stdlib (docs/STDLIB.md)
    ("slices", "slices.zl", include_str!("stdlib_src/slices.zl")),
    ("maps", "maps.zl", include_str!("stdlib_src/maps.zl")),
    ("cmp", "cmp.zl", include_str!("stdlib_src/cmp.zl")),
    ("bytes", "bytes.zl", include_str!("stdlib_src/bytes.zl")),
    ("strconv", "strconv.zl", include_str!("stdlib_src/strconv.zl")),
    ("unicode.utf8", "utf8.zl", include_str!("stdlib_src/utf8.zl")),
    ("unicode.utf16", "utf16.zl", include_str!("stdlib_src/utf16.zl")),
    ("math.bits", "bits.zl", include_str!("stdlib_src/bits.zl")),
    ("math.complex", "complex.zl", include_str!("stdlib_src/complex.zl")),
    ("math.big", "big.zl", include_str!("stdlib_src/big.zl")),
    ("hash", "hash.zl", include_str!("stdlib_src/hash.zl")),
    ("hash.fnv", "fnv.zl", include_str!("stdlib_src/fnv.zl")),
    ("hash.crc32", "crc32.zl", include_str!("stdlib_src/crc32.zl")),
    ("hash.crc64", "crc64.zl", include_str!("stdlib_src/crc64.zl")),
    ("hash.adler32", "adler32.zl", include_str!("stdlib_src/adler32.zl")),
    ("hash.maphash", "maphash.zl", include_str!("stdlib_src/maphash.zl")),
    ("encoding.binary", "binary.zl", include_str!("stdlib_src/binary.zl")),
    ("encoding.base32", "base32.zl", include_str!("stdlib_src/base32.zl")),
    ("encoding.pem", "pem.zl", include_str!("stdlib_src/pem.zl")),
    ("encoding.asn1", "asn1.zl", include_str!("stdlib_src/asn1.zl")),
    ("collections", "collections2.zl", include_str!("stdlib_src/collections2.zl")),
    ("sync", "sync.zl", include_str!("stdlib_src/sync.zl")),
    ("os.signal", "signal.zl", include_str!("stdlib_src/signal.zl")),
    ("process", "process.zl", include_str!("stdlib_src/process.zl")),
    ("os.user", "user.zl", include_str!("stdlib_src/user.zl")),
    ("cli", "cli.zl", include_str!("stdlib_src/cli.zl")),
    ("testing", "testing.zl", include_str!("stdlib_src/testing.zl")),
    ("embed", "embed.zl", include_str!("stdlib_src/embed.zl")),
    ("time", "time.zl", include_str!("stdlib_src/time.zl")),
    ("io", "io.zl", include_str!("stdlib_src/io.zl")),
    ("os", "os.zl", include_str!("stdlib_src/os.zl")),
    ("strings", "strings.zl", include_str!("stdlib_src/strings.zl")),
];

/// What registering a module needs of it: the names, kinds and type
/// parameters of its templates and its interfaces. The whole AST of a
/// module is only parsed when one of its templates is instantiated
/// ([`module_ast`]), so a program pays for the stdlib code it uses.
struct ModSummary {
    text: std::borrow::Cow<'static, str>,
    ns: String,
    decls: Vec<SumDecl>,
}

enum SumDecl {
    Struct(String, Vec<ast::GenericParam>),
    Func(String, Vec<ast::GenericParam>),
    Enum(String, Vec<ast::GenericParam>),
    /// With its spans erased (see [`parse_module`]).
    Interface(ast::InterfaceDecl),
}

/// The summaries of [`MODULES`] (parsed once per process).
fn summaries() -> &'static [ModSummary] {
    static SUMS: OnceLock<Vec<ModSummary>> = OnceLock::new();
    SUMS.get_or_init(|| {
        let _t = crate::build::phase("std-parse");
        // From `std/` on disk when installed (sema/stdfs.rs), in order (the
        // origins log keeps it); then lexed and parsed on several threads.
        let texts: Vec<std::borrow::Cow<'static, str>> = MODULES.iter().map(|(ns, name, embedded)| super::stdfs::load(ns, name, embedded)).collect();
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 8);
        let chunk = MODULES.len().div_ceil(n).max(1);
        let idx: Vec<usize> = (0..MODULES.len()).collect();
        let decls: Vec<Vec<SumDecl>> = std::thread::scope(|sc| {
            let hs: Vec<_> = idx
                .chunks(chunk)
                .map(|is| {
                    let texts = &texts;
                    sc.spawn(move || is.iter().map(|&i| summarize(parse_module(i, &texts[i]))).collect::<Vec<_>>())
                })
                .collect();
            hs.into_iter().flat_map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
        });
        super::stdfs::release_memory();
        texts.into_iter().zip(decls).zip(MODULES).map(|((text, decls), (ns, _, _))| ModSummary { text, ns: ns.to_string(), decls }).collect()
    })
}

fn summarize(file: ast::File) -> Vec<SumDecl> {
    let name = file.namespace.to_string();
    file.decls
        .into_iter()
        .map(|d| match d {
            ast::Decl::Struct(s) => SumDecl::Struct(s.name.name, s.generics),
            ast::Decl::Func(f) => SumDecl::Func(f.name.name, f.generics),
            ast::Decl::Enum(e) => SumDecl::Enum(e.name.name, e.generics),
            ast::Decl::Interface(it) => SumDecl::Interface(it),
            _ => panic!("internal error: unsupported declaration in stdlib source {name}"),
        })
        .collect()
}

/// The whole AST of module `MODULES[m]`, parsed on first use.
fn module_ast(m: usize) -> &'static ast::File {
    static ASTS: OnceLock<Vec<OnceLock<ast::File>>> = OnceLock::new();
    let asts = ASTS.get_or_init(|| (0..MODULES.len()).map(|_| OnceLock::new()).collect());
    asts[m].get_or_init(|| parse_module(m, &summaries()[m].text))
}

/// Placeholders for the declarations of templates whose module is not
/// parsed yet (only their kind is looked at, see [`Sema::force_template`]).
fn placeholders() -> &'static ast::File {
    static P: OnceLock<ast::File> = OnceLock::new();
    P.get_or_init(|| {
        let mut diags = crate::diagnostics::Diagnostics::new();
        let tokens = crate::lexer::lex("namespace placeholder\n\nfunc F() {}\n\nstruct S {}\n\nenum E {\n    A\n}\n", &mut diags);
        crate::parser::parse(tokens, &mut diags).expect("placeholder declarations parse")
    })
}

/// Parses stdlib module `MODULES[i]` from `src`; panics on errors.
fn parse_module(i: usize, src: &str) -> ast::File {
    let (ns, name, _) = MODULES[i];
    let mut diags = crate::diagnostics::Diagnostics::new();
    let tokens = crate::lexer::lex(src, &mut diags);
    let file = crate::parser::parse(tokens, &mut diags);
    match file {
        Some(mut f) if !diags.has_errors() && f.namespace.to_string() == ns => {
            // Interfaces are resolved like user declarations (in
            // the first user file's scope): without spans, so the
            // IDE index records nothing for them.
            for d in &mut f.decls {
                if let ast::Decl::Interface(it) = d {
                    erase_spans(it);
                }
            }
            f
        }
        _ => {
            let errs: Vec<String> = diags
                .items
                .iter()
                .map(|d| {
                    let line = src[..(d.span.start as usize).min(src.len())].matches('\n').count() + 1;
                    format!("{name}:{line}: {}", d.message)
                })
                .collect();
            panic!("internal error: stdlib source {name} does not parse: {errs:?}")
        }
    }
}

/// Replaces every span of an interface declaration with [`Span::DUMMY`].
fn erase_spans(it: &mut ast::InterfaceDecl) {
    fn ty(t: &mut ast::TypeExpr) {
        t.span = Span::DUMMY;
        match &mut t.kind {
            ast::TypeKind::Named { path, generics } => {
                path.span = Span::DUMMY;
                for s in &mut path.segments {
                    s.span = Span::DUMMY;
                }
                generics.iter_mut().for_each(ty);
            }
            ast::TypeKind::Fallible(x) | ast::TypeKind::Pointer(x) | ast::TypeKind::Weak(x) | ast::TypeKind::Slice(x) => ty(x),
            ast::TypeKind::Map(k, v) => {
                ty(k);
                ty(v);
            }
            ast::TypeKind::Func { params, ret } => {
                params.iter_mut().for_each(ty);
                if let Some(r) = ret {
                    ty(r);
                }
            }
            ast::TypeKind::Tuple(ts) => ts.iter_mut().for_each(ty),
            _ => {}
        }
    }
    it.name.span = Span::DUMMY;
    it.span = Span::DUMMY;
    for m in &mut it.methods {
        m.name.span = Span::DUMMY;
        m.span = Span::DUMMY;
        for p in &mut m.params {
            p.name.span = Span::DUMMY;
            p.span = Span::DUMMY;
            ty(&mut p.ty);
        }
        if let Some(r) = &mut m.ret {
            ty(r);
        }
    }
    it.embeds.iter_mut().for_each(ty);
}

impl<'a> Sema<'a> {
    /// Registers the declarations of the Zell-source stdlib modules.
    pub(crate) fn register_std_sources(&mut self) {
        if self.files.is_empty() {
            return;
        }
        let saved = (self.cur_file, self.cur_ns);
        let ph = placeholders();
        let (ph_func, ph_struct, ph_enum) = match ph.decls.as_slice() {
            [ast::Decl::Func(f), ast::Decl::Struct(s), ast::Decl::Enum(e)] => (f, s, e),
            _ => unreachable!("placeholder declarations"),
        };
        for (m, sum) in summaries().iter().enumerate() {
            let ns = self.ensure_ns(&sum.ns, Span::DUMMY);
            self.cur_ns = ns;
            self.cur_file = 0;
            for (di, d) in sum.decls.iter().enumerate() {
                let (name, t) = match d {
                    SumDecl::Struct(name, g) => (name, self.new_template(name, TmplDecl::Struct(ph_struct), g, Vec::new(), false)),
                    SumDecl::Func(name, g) => (name, self.new_template(name, TmplDecl::Func(ph_func), g, Vec::new(), false)),
                    SumDecl::Enum(name, g) => (name, self.new_template(name, TmplDecl::Enum(ph_enum), g, Vec::new(), false)),
                    SumDecl::Interface(it) => {
                        let id = self.interfaces.len() as u32;
                        self.interfaces.push(hir::Interface {
                            name: it.name.name.clone(),
                            full_name: format!("{}.{}", self.nss[ns].path, it.name.name),
                            namespace: hir::NamespaceId(0),
                            methods: Vec::new(),
                            sealed: it.is_sealed,
                            attrs: Vec::new(),
                            exported: true,
                            file: 0,
                            span: Span::DUMMY,
                        });
                        self.iface_src.push(it);
                        self.nss[ns].members.insert(it.name.name.clone(), (Member::Interface(id), Span::DUMMY, 0));
                        self.std_iface_lazy.insert(id, ns);
                        // Default methods: ordinary functions of the stdlib
                        // namespace (signature and body in its scope).
                        for m in it.methods.iter().filter(|m| m.body.is_some()) {
                            assert!(
                                !m.is_static && !m.is_async && m.generics.is_empty(),
                                "internal error: default method {}.{} in a stdlib source is static, async or generic",
                                it.name.name,
                                m.name.name
                            );
                            let fid = self.new_func(m, hir::FuncKind::Free, FuncSrc::Free(m), &format!("{}.{}", it.name.name, m.name.name));
                            self.iface_defaults.insert((id, m.name.name.clone()), fid);
                            self.iface_default_owner.insert(fid.0, id);
                            self.std_fn_ns.insert(fid.0, ns);
                        }
                        continue;
                    }
                };
                self.gen.templates[t as usize].lazy = Some((m as u32, di as u32));
                self.nss[ns].members.insert(name.clone(), (Member::Generic(t), Span::DUMMY, 0));
            }
        }
        (self.cur_file, self.cur_ns) = saved;
    }

    /// Gives template `t` of a stdlib module its declaration (parsing the
    /// module on first use); a no-op for other templates.
    pub(crate) fn force_template(&mut self, t: u32) {
        let Some((m, di)) = self.gen.templates[t as usize].lazy.take() else { return };
        let decl = match &module_ast(m as usize).decls[di as usize] {
            ast::Decl::Struct(s) => TmplDecl::Struct(s),
            ast::Decl::Func(f) => TmplDecl::Func(f),
            ast::Decl::Enum(e) => TmplDecl::Enum(e),
            _ => unreachable!("stdlib template declarations are structs, functions and enums"),
        };
        self.gen.templates[t as usize].decl = decl;
    }

    /// True if template `t` is a stdlib template without type parameters.
    fn is_plain_std(&self, t: u32) -> bool {
        let tm = &self.gen.templates[t as usize];
        tm.is_std && tm.params.is_empty()
    }

    /// Instantiates plain stdlib template `t` (no type parameters), or
    /// returns `None` if `t` has type parameters or is no stdlib template.
    pub(crate) fn plain_std_inst(&mut self, t: u32, span: Span) -> Option<Inst> {
        if !self.is_plain_std(t) {
            return None;
        }
        if let Some(i) = self.gen.cache.get(&(t, Vec::new())) {
            return Some(*i);
        }
        let funcs = self.funcs.len();
        let inst = self.instantiate(t, Vec::new(), span)?;
        // `Mutex<>` -> `Mutex` in names (symbols stay unique as they are).
        let fix = |s: &mut String| *s = s.replace("<>", "");
        match inst {
            Inst::Struct(s) => {
                let st = &mut self.structs[s as usize];
                fix(&mut st.name);
                fix(&mut st.full_name);
            }
            Inst::Enum(e) => {
                let en = &mut self.enums[e as usize];
                fix(&mut en.name);
                fix(&mut en.full_name);
            }
            Inst::Func(_) => {}
        }
        for f in &mut self.funcs[funcs..] {
            fix(&mut f.name);
            fix(&mut f.full_name);
        }
        Some(inst)
    }

    /// The type named by plain stdlib template `t` (`task.Mutex`).
    pub(crate) fn plain_std_type(&mut self, t: u32, span: Span) -> Option<TypeId> {
        if !self.is_plain_std(t) || !matches!(self.gen.templates[t as usize].decl, TmplDecl::Struct(_) | TmplDecl::Enum(_)) {
            return None;
        }
        Some(match self.plain_std_inst(t, span) {
            Some(Inst::Struct(s)) => self.struct_ty(s),
            Some(Inst::Enum(e)) => self.enum_ty(e),
            _ => self.tt.invalid,
        })
    }

    /// The struct or enum of plain stdlib type template `t` as a member
    /// (for paths through it: `log.Level.Debug`, `log.Context.Push`).
    pub(crate) fn plain_std_member(&mut self, t: u32, span: Span) -> Option<Member> {
        if !self.is_plain_std(t) || !matches!(self.gen.templates[t as usize].decl, TmplDecl::Struct(_) | TmplDecl::Enum(_)) {
            return None;
        }
        match self.plain_std_inst(t, span)? {
            Inst::Struct(s) => Some(Member::Struct(s)),
            Inst::Enum(e) => Some(Member::Enum(e)),
            Inst::Func(_) => None,
        }
    }

    /// The struct of plain stdlib struct template `t` (literals `task.Mutex{}`).
    pub(crate) fn plain_std_struct(&mut self, t: u32, span: Span) -> Option<u32> {
        if !matches!(self.gen.templates[t as usize].decl, TmplDecl::Struct(_)) {
            return None;
        }
        match self.plain_std_inst(t, span)? {
            Inst::Struct(s) => Some(s),
            _ => None,
        }
    }

    /// The function of plain stdlib function template `t`, as a value.
    pub(crate) fn plain_std_func_value(&mut self, t: u32, span: Span) -> Option<hir::Expr> {
        if !matches!(self.gen.templates[t as usize].decl, TmplDecl::Func(_)) {
            return None;
        }
        match self.plain_std_inst(t, span)? {
            Inst::Func(f) => {
                let ty = self.func_value_ty(f);
                Some(mk(hir::ExprKind::FuncRef(f), ty, span))
            }
            _ => None,
        }
    }

    /// `task.Channel<int>(8)`: calling a stdlib generic struct type calls
    /// its static `New` method. `None` if `t` is no stdlib struct template.
    pub(crate) fn std_type_call(&mut self, t: u32, targs: Option<&[ast::TypeExpr]>, args: &[ast::Expr], span: Span) -> Option<hir::Expr> {
        let tm = &self.gen.templates[t as usize];
        if !tm.is_std || !matches!(tm.decl, TmplDecl::Struct(_)) {
            return None;
        }
        let name = tm.name.clone();
        let sid = match targs {
            Some(ta) => {
                let Some(ts) = self.type_args(t, ta, span) else { return Some(self.invalid(span)) };
                match self.instantiate(t, ts, span) {
                    Some(Inst::Struct(s)) => s,
                    _ => return Some(self.invalid(span)),
                }
            }
            None => match self.plain_std_inst(t, span) {
                Some(Inst::Struct(s)) => s,
                _ => {
                    self.err(format!("missing type arguments for generic type {name} (write {name}<...>(...))"), span);
                    self.check_args_loose(args);
                    return Some(self.invalid(span));
                }
            },
        };
        let ty = self.struct_ty(sid);
        let new = ast::Ident { name: "New".to_string(), span };
        Some(self.type_member_call(ty, &new, args, span))
    }
}
