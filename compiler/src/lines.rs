//! The line table of panic backtraces (runtime/ABI.md 8.2).
//!
//! Codegen emits DWARF line tables in every build ([`crate::codegen::Options::line_tables`]).
//! Before the program's objects are written, [`expose`] gives every
//! function definition external linkage (hidden visibility), so another
//! object can name it. Before the link, [`write_object`] reads `.debug_line`
//! of those objects (addresses relative to their sections, through the
//! relocations), maps every row to the function symbol holding it, and
//! emits a second, small object: one read-only global `zell_lines` (plus
//! `zell_lines_len`) and a constructor that registers it with the runtime
//! (`zell_lines_register`). Every function's address in the table is a
//! 32-bit offset from the table (`ptrtoint(@f) - ptrtoint(&slot)`, magic
//! "ZLN2"), which the static linker resolves on ELF, COFF and arm64 Mach-O.
//! x86_64 Mach-O cannot encode that difference (`@f` is undefined in this
//! object: "unsupported relocation with subtraction expression"), so there
//! each entry is an absolute 64-bit pointer instead (magic "ZLN3",
//! `X86_64_RELOC_UNSIGNED`, rebased by dyld). The table is ordinary data,
//! so `strip` keeps it and no load bias applies in either form.
//!
//! ```text
//! "ZLN2"  u32 nfiles  u32 nlines  u32 nfuncs  u32 strsize  u32 ncode
//! {i32 rel; u32 size; u32 first} code[ncode]   one per function: rel = its address - the address of rel;
//!                                              its line entries are lines[first .. next code's first)
//!                                              ("ZLN3": {u64 addr; u32 size; u32 first}, table 8-aligned)
//! u32 files[nfiles]                            string offsets (paths as given to the compiler)
//! {u32 off; u32 fl} lines[nlines]              off from the function's start, sorted; fl = file << 20 | line, file 0xfff: no line
//! {u32 fl; u32 last; u32 name} funcs[nfuncs]   source range of a Zell function: file << 20 | first line, last line
//! char str[strsize]
//! ```
//!
//! An entry covers addresses up to the next one of its function. One line
//! entry per statement location LLVM kept (what DWARF's line program has),
//! consecutive duplicates merged; code inlined into another function keeps
//! its own lines. A frame's function is the innermost function whose
//! source range holds its line (exact with inlining, unlike the symbol of
//! the address).

use crate::hir::Program;
use crate::target::Target;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::targets::{FileType, TargetMachine};
use inkwell::GlobalVisibility;
use object::{Object, ObjectSection, ObjectSymbol};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Whether `target` gets the table: every 64-bit target whose objects
/// carry DWARF (ELF, Mach-O, MinGW COFF; not the MSVC ABI's CodeView).
pub fn supported(t: &Target) -> bool {
    !t.is_msvc() && matches!(t.os.as_str(), "linux" | "freebsd" | "macos" | "windows") && matches!(t.arch.as_str(), "amd64" | "arm64")
}

/// What the table names: source files and Zell functions.
#[derive(Default)]
pub struct Names {
    /// Absolute path (as in the DWARF) -> path as printed (as in error traces).
    files: HashMap<String, String>,
    /// (path as printed, first line, last line, name as in error traces).
    funcs: Vec<(String, u32, u32, String)>,
}

impl Names {
    /// The files and functions of `prog`.
    pub fn of(prog: &Program) -> Names {
        let mut n = Names::default();
        for src in &prog.sources {
            let abs = crate::paths::canonical_or_absolute(Path::new(&src.path));
            n.files.insert(abs.to_string_lossy().into_owned(), src.path.clone());
        }
        for f in &prog.funcs {
            let Some(src) = prog.sources.get(f.file) else { continue };
            if crate::sema::stdlib::is_std_namespace(&f.full_name) || f.span == crate::span::Span::DUMMY {
                continue;
            }
            let ns = &prog.namespaces[f.namespace.0 as usize].path;
            let name = f.full_name.strip_prefix(&format!("{ns}.")).unwrap_or(&f.full_name);
            n.funcs.push((src.path.clone(), src.line_of(f.span.start), src.line_of(f.span.end.saturating_sub(1).max(f.span.start)), name.to_string()));
        }
        n
    }
}

/// Gives every internal or private function definition of `module`
/// external linkage with hidden visibility, so the line-table object can
/// refer to it (after optimisation, so nothing is lost: the symbols stay
/// out of the dynamic symbol table and stripped builds drop them). `tag`
/// (package builds: several objects) is appended to their names to keep
/// them unique across objects.
pub fn expose(module: &Module, tag: Option<&str>) {
    for f in module.get_functions() {
        if f.count_basic_blocks() == 0 {
            continue;
        }
        let gv = f.as_global_value();
        if matches!(gv.get_linkage(), Linkage::Internal | Linkage::Private) {
            if let Some(tag) = tag {
                let n = gv.get_name().to_string_lossy().into_owned();
                gv.set_name(&format!("{n}.{tag}"));
            }
            gv.set_linkage(Linkage::External);
            gv.set_visibility(GlobalVisibility::Hidden);
        }
    }
}

const NONE_FILE: u32 = 0xfff;
/// Row addresses are rewritten to `(section index + 1) << SEC_SHIFT | offset`.
const SEC_SHIFT: u32 = 40;

/// One function of the table: its symbol, size and line entries
/// `(offset from its start, file << 20 | line)`.
struct Code {
    sym: String,
    size: u64,
    lines: Vec<(u32, u32)>,
}

/// The file table being built: paths as printed, by index.
#[derive(Default)]
struct Files {
    list: Vec<String>,
    index: HashMap<String, u32>,
}

impl Files {
    /// Index of the Zell source `dir`/`name` (NONE_FILE if it is not one).
    fn id(&mut self, names: &Names, dir: &str, name: &str) -> u32 {
        let full = if dir.is_empty() || Path::new(name).is_absolute() { PathBuf::from(name) } else { Path::new(dir).join(name) };
        let full = full.to_string_lossy();
        let Some(shown) = names.files.get(full.as_ref()).or_else(|| names.files.get(&full.replace('\\', "/"))) else { return NONE_FILE };
        if let Some(&i) = self.index.get(shown) {
            return i;
        }
        if self.list.len() >= NONE_FILE as usize {
            return NONE_FILE;
        }
        self.list.push(shown.clone());
        self.index.insert(shown.clone(), self.list.len() as u32 - 1);
        self.list.len() as u32 - 1
    }
}

/// The functions with lines of object file `path`.
fn object_code(path: &Path, names: &Names, files: &mut Files, out: &mut Vec<Code>) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let obj = object::File::parse(&*data).map_err(|e| format!("{}: {e}", path.display()))?;
    let macho = obj.format() == object::BinaryFormat::MachO;
    let sec_addr = |i: object::SectionIndex| obj.section_by_index(i).map(|s| s.address()).unwrap_or(0);
    // Function symbols by section: (offset, size or 0, name), global ones only.
    let mut syms: HashMap<usize, Vec<(u64, u64, String)>> = HashMap::new();
    for s in obj.symbols() {
        let object::SymbolSection::Section(si) = s.section() else { continue };
        if s.kind() != object::SymbolKind::Text || !s.is_global() || s.is_undefined() {
            continue;
        }
        let Ok(name) = s.name() else { continue };
        // Mach-O symbol names carry the C prefix `_`, which LLVM adds again.
        let name = if macho { name.strip_prefix('_').unwrap_or(name) } else { name };
        syms.entry(si.0).or_default().push((s.address() - sec_addr(si), s.size(), name.to_string()));
    }
    for (si, v) in syms.iter_mut() {
        v.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        v.dedup_by_key(|s| s.0);
        let end = obj.section_by_index(object::SectionIndex(*si)).map(|s| s.size()).unwrap_or(0);
        for i in 0..v.len() {
            if v[i].1 == 0 {
                v[i].1 = v.get(i + 1).map_or(end, |n| n.0) - v[i].0;
            }
        }
    }
    let section = |names: &[&str]| names.iter().find_map(|n| obj.section_by_name(n));
    let Some(dl_sec) = section(&[".debug_line", "__debug_line"]) else { return Ok(()) };
    let mut dl = dl_sec.uncompressed_data().map_err(|e| format!("{}: {e}", path.display()))?.into_owned();
    // Apply the relocations: code addresses become (section, offset) pairs,
    // other references (string offsets) plain section offsets.
    for (off, rel) in dl_sec.relocations() {
        let (off, size) = (off as usize, rel.size() as usize / 8);
        if !(size == 4 || size == 8) || off + size > dl.len() {
            continue;
        }
        let inplace = {
            let mut b = [0u8; 8];
            b[..size].copy_from_slice(&dl[off..off + size]);
            if size == 4 {
                u32::from_le_bytes(b[..4].try_into().unwrap()) as i32 as i64
            } else {
                i64::from_le_bytes(b)
            }
        };
        let addend = if rel.has_implicit_addend() { inplace } else { rel.addend() };
        let (si, val) = match rel.target() {
            object::RelocationTarget::Symbol(i) => {
                let Ok(s) = obj.symbol_by_index(i) else { continue };
                let object::SymbolSection::Section(si) = s.section() else { continue };
                (si, (s.address() - sec_addr(si)) as i64 + addend)
            }
            // Mach-O section relocations hold the target's address in the object.
            object::RelocationTarget::Section(si) => (si, if macho { addend - sec_addr(si) as i64 } else { addend }),
            _ => continue,
        };
        let code = obj.section_by_index(si).is_ok_and(|s| s.kind() == object::SectionKind::Text);
        let v = if code { ((si.0 as u64 + 1) << SEC_SHIFT) | (val as u64 & ((1u64 << SEC_SHIFT) - 1)) } else { val as u64 };
        dl[off..off + size].copy_from_slice(&v.to_le_bytes()[..size]);
    }
    let empty: &[u8] = &[];
    let line_str = section(&[".debug_line_str", "__debug_line_str"]).and_then(|s| s.uncompressed_data().ok()).map(|c| c.into_owned()).unwrap_or_default();
    let strs = section(&[".debug_str", "__debug_str"]).and_then(|s| s.uncompressed_data().ok()).map(|c| c.into_owned()).unwrap_or_default();
    let le = gimli::LittleEndian;
    let debug_line = gimli::DebugLine::new(&dl, le);
    let debug_line_str = gimli::DebugLineStr::new(if line_str.is_empty() { empty } else { &line_str }, le);
    let debug_str = gimli::DebugStr::new(if strs.is_empty() { empty } else { &strs }, le);
    let string = |v: gimli::AttributeValue<gimli::EndianSlice<gimli::LittleEndian>>| -> String {
        let r = match v {
            gimli::AttributeValue::String(s) => Some(s),
            gimli::AttributeValue::DebugLineStrRef(o) => debug_line_str.get_str(o).ok(),
            gimli::AttributeValue::DebugStrRef(o) => debug_str.get_str(o).ok(),
            _ => None,
        };
        r.map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    };
    // Rows: (section, offset, file << 20 | line).
    let mut rows: Vec<(usize, u64, u32)> = Vec::new();
    let mut p = 0usize;
    while p + 4 <= dl.len() {
        let len = u32::from_le_bytes(dl[p..p + 4].try_into().unwrap()) as usize;
        if len >= 0xffff_fff0 {
            break; // 64-bit DWARF: not produced by LLVM by default
        }
        let unit = p;
        p += 4 + len;
        let Ok(prog) = debug_line.program(gimli::DebugLineOffset(unit), 8, None, None) else { continue };
        let mut fcache: HashMap<u64, u32> = HashMap::new();
        let mut it = prog.rows();
        while let Ok(Some((header, row))) = it.next_row() {
            let a = row.address();
            if a >> SEC_SHIFT == 0 {
                continue;
            }
            let (si, off) = ((a >> SEC_SHIFT) as usize - 1, a & ((1u64 << SEC_SHIFT) - 1));
            let line = row.line().map_or(0, |l| l.get()).min(0xfffff) as u32;
            let fi = if row.end_sequence() || line == 0 {
                NONE_FILE
            } else {
                *fcache.entry(row.file_index()).or_insert_with(|| match row.file(header) {
                    Some(f) => {
                        let dir = f.directory(header).map(&string).unwrap_or_default();
                        files.id(names, &dir, &string(f.path_name()))
                    }
                    None => NONE_FILE,
                })
            };
            rows.push((si, off, if fi == NONE_FILE { NONE_FILE << 20 } else { fi << 20 | line }));
        }
    }
    // Rows by function.
    let mut by_fn: HashMap<(usize, usize), Vec<(u32, u32)>> = HashMap::new();
    for (si, off, fl) in rows {
        let Some(v) = syms.get(&si) else { continue };
        let i = v.partition_point(|s| s.0 <= off);
        if i == 0 || off - v[i - 1].0 >= v[i - 1].1 {
            continue;
        }
        by_fn.entry((si, i - 1)).or_default().push(((off - v[i - 1].0) as u32, fl));
    }
    let mut keys: Vec<(usize, usize)> = by_fn.keys().copied().collect();
    keys.sort();
    for k in keys {
        let mut l = by_fn.remove(&k).unwrap();
        l.sort_by_key(|e| e.0); // stable: of rows at one address the last wins
        let mut lines: Vec<(u32, u32)> = Vec::new();
        for (o, fl) in l {
            match lines.last_mut() {
                Some(last) if last.0 == o => last.1 = fl,
                _ => lines.push((o, fl)),
            }
        }
        lines.dedup_by(|b, a| a.1 == b.1);
        if lines.iter().all(|e| e.1 >> 20 == NONE_FILE) {
            continue;
        }
        let (_, size, sym) = &syms[&k.0][k.1];
        out.push(Code { sym: sym.clone(), size: *size, lines });
    }
    Ok(())
}

/// Builds the line table of the program objects `objs` and writes the
/// line-table object (for `machine`'s target) to `out`. Returns the
/// table's size, or `None` (nothing written) when no Zell code has lines.
pub fn write_object(objs: &[PathBuf], names: &Names, machine: &TargetMachine, out: &Path) -> Result<Option<usize>, String> {
    let mut files = Files::default();
    let mut code = Vec::new();
    for o in objs {
        object_code(o, names, &mut files, &mut code)?;
    }
    if code.is_empty() {
        return Ok(None);
    }
    write_table(&code, &files, names, machine, out).map(Some)
}

/// Writes the line-table object of `code` (whose files are `files`) to `out`;
/// returns the table's size.
fn write_table(code: &[Code], files: &Files, names: &Names, machine: &TargetMachine, out: &Path) -> Result<usize, String> {
    // Strings, functions' source ranges, the tail of the table.
    let mut strtab: Vec<u8> = Vec::new();
    let mut strs: HashMap<String, u32> = HashMap::new();
    let mut intern = |s: &str, strtab: &mut Vec<u8>| -> u32 {
        if let Some(&o) = strs.get(s) {
            return o;
        }
        let o = strtab.len() as u32;
        strtab.extend_from_slice(s.as_bytes());
        strtab.push(0);
        strs.insert(s.to_string(), o);
        o
    };
    let mut funcs: Vec<(u32, u32, u32)> = Vec::new();
    for (path, lo, hi, name) in &names.funcs {
        if let Some(&fi) = files.index.get(path) {
            funcs.push((fi << 20 | (*lo).min(0xfffff), *hi, intern(name, &mut strtab)));
        }
    }
    funcs.sort();
    funcs.dedup();
    let file_offs: Vec<u32> = files.list.iter().map(|p| intern(p, &mut strtab)).collect();
    let nlines: usize = code.iter().map(|c| c.lines.len()).sum();
    let mut tail: Vec<u8> = Vec::new();
    for o in &file_offs {
        tail.extend_from_slice(&o.to_le_bytes());
    }
    for c in code {
        for (o, v) in &c.lines {
            tail.extend_from_slice(&o.to_le_bytes());
            tail.extend_from_slice(&v.to_le_bytes());
        }
    }
    for (fl, hi, n) in &funcs {
        for v in [fl, hi, n] {
            tail.extend_from_slice(&v.to_le_bytes());
        }
    }
    tail.extend_from_slice(&strtab);
    // The module: zell_lines = { header, code[], tail }.
    let ctx = Context::create();
    let m = ctx.create_module("zell_lines");
    m.set_triple(&machine.get_triple());
    m.set_data_layout(&machine.get_target_data().get_data_layout());
    let (i32t, i64t, i8t, ptr) = (ctx.i32_type(), ctx.i64_type(), ctx.i8_type(), ctx.ptr_type(inkwell::AddressSpace::default()));
    // x86_64 Mach-O cannot encode `@f - &slot` with `@f` undefined in this
    // object (only arm64's SUBTRACTOR pair can): store absolute pointers,
    // rebased by dyld ("ZLN3"); elsewhere 32-bit offsets ("ZLN2").
    let abs = machine.get_triple().as_str().to_string_lossy().starts_with("x86_64-apple");
    let code_ty = if abs { ctx.struct_type(&[ptr.into(), i32t.into(), i32t.into()], false) } else { ctx.struct_type(&[i32t.into(), i32t.into(), i32t.into()], false) };
    let tab_ty = ctx.struct_type(&[i32t.array_type(6).into(), code_ty.array_type(code.len() as u32).into(), i8t.array_type(tail.len() as u32).into()], false);
    let g = m.add_global(tab_ty, None, "zell_lines");
    let magic = u32::from_le_bytes(if abs { *b"ZLN3" } else { *b"ZLN2" });
    let hdr = [magic, file_offs.len() as u32, nlines as u32, funcs.len() as u32, strtab.len() as u32, code.len() as u32];
    let hdr = i32t.const_array(&hdr.iter().map(|v| i32t.const_int(*v as u64, false)).collect::<Vec<_>>());
    let fn_ty = ctx.void_type().fn_type(&[], false);
    let mut entries = Vec::new();
    let mut first = 0u32;
    for (k, c) in code.iter().enumerate() {
        let f = m.get_function(&c.sym).unwrap_or_else(|| m.add_function(&c.sym, fn_ty, Some(Linkage::External)));
        let fp = f.as_global_value().as_pointer_value();
        let addr: inkwell::values::BasicValueEnum = if abs {
            fp.into()
        } else {
            // SAFETY: indices within the table's type.
            let slot = unsafe { g.as_pointer_value().const_in_bounds_gep(tab_ty, &[i32t.const_zero(), i32t.const_int(1, false), i32t.const_int(k as u64, false), i32t.const_zero()]) };
            fp.const_to_int(i64t).const_sub(slot.const_to_int(i64t)).const_truncate(i32t).into()
        };
        entries.push(code_ty.const_named_struct(&[addr, i32t.const_int(c.size.min(u32::MAX as u64), false).into(), i32t.const_int(first as u64, false).into()]));
        first += c.lines.len() as u32;
    }
    let bytes: Vec<_> = tail.iter().map(|b| i8t.const_int(*b as u64, false)).collect();
    g.set_initializer(&tab_ty.const_named_struct(&[hdr.into(), code_ty.const_array(&entries).into(), i8t.const_array(&bytes).into()]));
    g.set_constant(true);
    g.set_alignment(if abs { 8 } else { 4 });
    g.set_visibility(GlobalVisibility::Hidden);
    let size = tail.len() + 24 + if abs { 16 } else { 12 } * code.len();
    let len = m.add_global(i64t, None, "zell_lines_len");
    len.set_initializer(&i64t.const_int(size as u64, false));
    len.set_constant(true);
    len.set_visibility(GlobalVisibility::Hidden);
    // The constructor: zell_lines_register(&zell_lines, size).
    let reg = m.add_function("zell_lines_register", ctx.void_type().fn_type(&[ptr.into(), i64t.into()], false), Some(Linkage::External));
    let ctor = m.add_function("zell_lines.init", fn_ty, Some(Linkage::Internal));
    let b = ctx.create_builder();
    b.position_at_end(ctx.append_basic_block(ctor, "entry"));
    b.build_call(reg, &[g.as_pointer_value().into(), i64t.const_int(size as u64, false).into()], "").map_err(|e| e.to_string())?;
    b.build_return(None).map_err(|e| e.to_string())?;
    let ctor_ty = ctx.struct_type(&[i32t.into(), ptr.into(), ptr.into()], false);
    let entry = ctor_ty.const_named_struct(&[i32t.const_int(65535, false).into(), ctor.as_global_value().as_pointer_value().into(), ptr.const_null().into()]);
    let ctors = m.add_global(ctor_ty.array_type(1), None, "llvm.global_ctors");
    ctors.set_linkage(Linkage::Appending);
    ctors.set_initializer(&ctor_ty.const_array(&[entry]));
    if let Err(e) = m.verify() {
        return Err(format!("internal compiler error: line-table module is invalid:\n{}", e.to_string()));
    }
    machine.write_to_file(&m, FileType::Object, out).map_err(|e| format!("cannot write {}: {}", out.display(), e.to_string()))?;
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line-table object is emitted for every supported object format
    /// (x86_64 Mach-O refuses a difference with a symbol undefined in the object).
    #[test]
    fn table_object_every_target() {
        let dir = std::env::temp_dir().join(format!("zell-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let code: Vec<Code> = ["main.f", "main.g"].iter().map(|s| Code { sym: s.to_string(), size: 16, lines: vec![(0, 7), (4, 8)] }).collect();
        let mut files = Files::default();
        files.list.push("t.zl".into());
        files.index.insert("t.zl".into(), 0);
        for t in ["macos-amd64", "macos-arm64", "linux-amd64", "linux-arm64", "windows-amd64"] {
            let target = Target::parse(t).unwrap();
            let machine = crate::codegen::target_machine(&target, inkwell::OptimizationLevel::None).unwrap();
            let out = dir.join(format!("{t}.o"));
            let r = write_table(&code, &files, &Names::default(), &machine, &out);
            assert!(r.is_ok(), "{t}: {r:?}");
            let data = std::fs::read(&out).unwrap();
            let obj = object::File::parse(&*data).unwrap();
            assert!(obj.symbols().any(|s| s.name().is_ok_and(|n| n.ends_with("zell_lines"))), "{t}: no zell_lines");
            let magic: &[u8] = if t == "macos-amd64" { b"ZLN3" } else { b"ZLN2" };
            assert!(data.windows(4).any(|w| w == magic), "{t}: no {magic:?} table");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
