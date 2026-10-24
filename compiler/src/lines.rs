//! The `.zell_lines` table of panic backtraces (runtime/ABI.md 8.2).
//!
//! Codegen emits DWARF line tables in every build for ELF targets
//! ([`crate::codegen::Options::line_tables`]). After the link, [`finish`]
//! reads the executable's `.debug_line` (and the HIR's function ranges), writes the compact
//! table below into a non-allocated `.zell_lines` section and drops what
//! the build did not ask for: the DWARF sections without `-g`, the symbol
//! table of stripped builds (Zell strips ELF executables itself, instead of
//! the linker's `-s`). The table is data, so `strip(1)` keeps it.
//!
//! ```text
//! "ZLN1"  u32 nfiles  u32 nlines  u32 nfuncs  u32 strsize  u32 0  u64 base
//! u32 files[nfiles]                   string offsets (paths as given to the compiler)
//! {u32 off; u32 fl} lines[nlines]     sorted by off; fl = file << 20 | line, file 0xfff: no line
//! {u32 fl; u32 last; u32 name} funcs[nfuncs]  source range of a function: file << 20 | first line, last line
//! char str[strsize]
//! ```
//!
//! `off` is a link-time address minus `base`; an entry covers addresses up
//! to the next entry. One line entry per statement location LLVM kept
//! (what DWARF's line program has), consecutive duplicates merged; code
//! inlined into another function keeps its own lines. A frame's function
//! is the innermost function whose source range holds its line (exact with
//! inlining, unlike the symbol of the address).

use crate::hir::Program;
use crate::target::Target;
use std::collections::HashMap;
use std::path::Path;

/// Whether `target` gets the table (ELF64 little-endian executables).
pub fn supported(t: &Target) -> bool {
    matches!(t.os.as_str(), "linux" | "freebsd") && matches!(t.arch.as_str(), "amd64" | "arm64")
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

/// Little-endian reader over a byte slice.
struct R<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn n(&mut self, k: usize) -> Option<u64> {
        let s = self.b.get(self.p..self.p + k)?;
        self.p += k;
        Some(s.iter().rev().fold(0u64, |a, &x| a << 8 | x as u64))
    }
    fn uleb(&mut self) -> Option<u64> {
        let (mut v, mut sh) = (0u64, 0);
        loop {
            let x = self.u8()?;
            if sh < 64 {
                v |= ((x & 0x7f) as u64) << sh;
            }
            sh += 7;
            if x & 0x80 == 0 {
                return Some(v);
            }
        }
    }
    fn sleb(&mut self) -> Option<i64> {
        let (mut v, mut sh) = (0i64, 0);
        loop {
            let x = self.u8()?;
            if sh < 64 {
                v |= ((x & 0x7f) as i64) << sh;
            }
            sh += 7;
            if x & 0x80 == 0 {
                if sh < 64 && x & 0x40 != 0 {
                    v |= -1i64 << sh;
                }
                return Some(v);
            }
        }
    }
    fn cstr(&mut self) -> Option<String> {
        let rest = self.b.get(self.p..)?;
        let end = rest.iter().position(|&c| c == 0)?;
        self.p += end + 1;
        Some(String::from_utf8_lossy(&rest[..end]).into_owned())
    }
}

fn cstr_at(b: &[u8], off: u64) -> String {
    let Some(rest) = b.get(off as usize..) else { return String::new() };
    let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end]).into_owned()
}

/// One ELF section header (ELF64).
#[derive(Clone)]
struct Sh {
    name: String,
    raw: [u8; 64],
}

impl Sh {
    fn get(&self, off: usize, k: usize) -> u64 {
        R { b: &self.raw, p: off }.n(k).unwrap_or(0)
    }
    fn set(&mut self, off: usize, k: usize, v: u64) {
        self.raw[off..off + k].copy_from_slice(&v.to_le_bytes()[..k]);
    }
    fn ty(&self) -> u32 {
        self.get(4, 4) as u32
    }
    fn flags(&self) -> u64 {
        self.get(8, 8)
    }
    fn addr(&self) -> u64 {
        self.get(16, 8)
    }
    fn offset(&self) -> u64 {
        self.get(24, 8)
    }
    fn size(&self) -> u64 {
        self.get(32, 8)
    }
    fn link(&self) -> u32 {
        self.get(40, 4) as u32
    }
    fn data<'a>(&self, f: &'a [u8]) -> &'a [u8] {
        if self.ty() == 8 {
            return &[];
        }
        f.get(self.offset() as usize..(self.offset() + self.size()) as usize).unwrap_or(&[])
    }
}

const SHF_ALLOC: u64 = 2;
const SHF_EXECINSTR: u64 = 4;
const NONE_FILE: u32 = 0xfff;

/// Rows (address, file index into `files` or NONE_FILE, line) of every
/// line program in `.debug_line`.
fn line_rows(dl: &[u8], line_str: &[u8], strs: &[u8], names: &Names, files: &mut Vec<String>, exec: &dyn Fn(u64) -> bool) -> Vec<(u64, u32, u32)> {
    let mut out = Vec::new();
    let mut index: HashMap<String, u32> = HashMap::new();
    let mut p = 0usize;
    while p + 4 <= dl.len() {
        let mut r = R { b: dl, p };
        let Some(len) = r.n(4) else { break };
        if len >= 0xffff_fff0 {
            break; // 64-bit DWARF: not produced by LLVM by default
        }
        let end = r.p + len as usize;
        p = end;
        if end > dl.len() {
            break;
        }
        if unit_rows(&mut R { b: &dl[..end], p: r.p }, line_str, strs, names, files, &mut index, exec, &mut out).is_none() {
            continue;
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn unit_rows(r: &mut R, line_str: &[u8], strs: &[u8], names: &Names, files: &mut Vec<String>, index: &mut HashMap<String, u32>, exec: &dyn Fn(u64) -> bool, out: &mut Vec<(u64, u32, u32)>) -> Option<()> {
    let ver = r.n(2)?;
    if !(2..=5).contains(&ver) {
        return None;
    }
    let mut asz = 8;
    if ver >= 5 {
        asz = r.u8()? as usize;
        r.u8()?;
    }
    let hlen = r.n(4)? as usize;
    let prog = r.p + hlen;
    let min_inst = r.u8()? as u64;
    if ver >= 4 {
        r.u8()?;
    }
    r.u8()?; // default_is_stmt
    let line_base = r.u8()? as i8 as i64;
    let line_range = r.u8()? as u64;
    let opcode_base = r.u8()?;
    let mut std_len = vec![0u8; opcode_base.max(1) as usize];
    for l in std_len.iter_mut().skip(1) {
        *l = r.u8()?;
    }
    if line_range == 0 {
        return None;
    }
    // File table -> index in `files` (NONE_FILE when not a Zell source).
    let mut ftab: Vec<u32> = Vec::new();
    let mut map_file = |dir: &str, name: &str, files: &mut Vec<String>| -> u32 {
        let full = if name.starts_with('/') || dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
        let Some(shown) = names.files.get(&full) else { return NONE_FILE };
        if let Some(&i) = index.get(shown) {
            return i;
        }
        if files.len() >= NONE_FILE as usize {
            return NONE_FILE;
        }
        files.push(shown.clone());
        index.insert(shown.clone(), files.len() as u32 - 1);
        files.len() as u32 - 1
    };
    if ver >= 5 {
        let form_str = |r: &mut R, form: u64| -> Option<Option<String>> {
            Some(match form {
                0x08 => Some(r.cstr()?),
                0x1f => Some(cstr_at(line_str, r.n(4)?)),
                0x0e => Some(cstr_at(strs, r.n(4)?)),
                0x0f => {
                    r.uleb()?;
                    None
                }
                0x0b => {
                    r.n(1)?;
                    None
                }
                0x05 => {
                    r.n(2)?;
                    None
                }
                0x06 => {
                    r.n(4)?;
                    None
                }
                0x07 => {
                    r.n(8)?;
                    None
                }
                0x1e => {
                    r.n(8)?;
                    r.n(8)?;
                    None
                }
                0x09 => {
                    let k = r.uleb()? as usize;
                    r.p += k;
                    None
                }
                _ => return None,
            })
        };
        let read_fmt = |r: &mut R| -> Option<Vec<(u64, u64)>> {
            let k = r.u8()?;
            (0..k).map(|_| Some((r.uleb()?, r.uleb()?))).collect()
        };
        let dfmt = read_fmt(r)?;
        let nd = r.uleb()?;
        let mut dirs = Vec::new();
        for _ in 0..nd {
            let mut path = String::new();
            for &(ct, form) in &dfmt {
                let v = form_str(r, form)?;
                if ct == 1 {
                    path = v.unwrap_or_default();
                }
            }
            dirs.push(path);
        }
        let ffmt = read_fmt(r)?;
        let nf = r.uleb()?;
        for _ in 0..nf {
            let (mut name, mut di) = (String::new(), 0u64);
            for &(ct, form) in &ffmt {
                if ct == 2 {
                    di = match form {
                        0x0b => r.n(1)?,
                        0x05 => r.n(2)?,
                        _ => r.uleb()?,
                    };
                    continue;
                }
                let v = form_str(r, form)?;
                if ct == 1 {
                    name = v.unwrap_or_default();
                }
            }
            let dir = dirs.get(di as usize).cloned().unwrap_or_default();
            let i = map_file(&dir, &name, files);
            ftab.push(i);
        }
    } else {
        let mut dirs = vec![String::new()];
        loop {
            let d = r.cstr()?;
            if d.is_empty() {
                break;
            }
            dirs.push(d);
        }
        ftab.push(NONE_FILE); // DWARF < 5 numbers files from 1
        loop {
            let n = r.cstr()?;
            if n.is_empty() {
                break;
            }
            let di = r.uleb()?;
            r.uleb()?;
            r.uleb()?;
            let dir = dirs.get(di as usize).cloned().unwrap_or_default();
            let i = map_file(&dir, &n, files);
            ftab.push(i);
        }
    }
    r.p = prog;
    // The line program.
    let (mut addr, mut file, mut line) = (0u64, 1u64, 1i64);
    let mut seq: Vec<(u64, u32, u32)> = Vec::new();
    let fl = |file: u64, line: i64| -> (u32, u32) {
        let f = ftab.get(file as usize).copied().unwrap_or(NONE_FILE);
        if f == NONE_FILE || line <= 0 {
            (NONE_FILE, 0)
        } else {
            (f, line.min(0xfffff) as u32)
        }
    };
    while r.p < r.b.len() {
        let op = r.u8()?;
        if op >= opcode_base {
            let adj = (op - opcode_base) as u64;
            addr += (adj / line_range) * min_inst;
            line += line_base + (adj % line_range) as i64;
            let (f, l) = fl(file, line);
            seq.push((addr, f, l));
            continue;
        }
        match op {
            0 => {
                let len = r.uleb()? as usize;
                let next = r.p + len;
                match r.u8()? {
                    1 => {
                        seq.push((addr, NONE_FILE, 0));
                        if seq.first().is_some_and(|s| exec(s.0)) {
                            out.append(&mut seq);
                        }
                        seq.clear();
                        addr = 0;
                        file = 1;
                        line = 1;
                    }
                    2 => addr = r.n(asz)?,
                    _ => {}
                }
                r.p = next;
            }
            1 => {
                let (f, l) = fl(file, line);
                seq.push((addr, f, l));
            }
            2 => addr += r.uleb()? * min_inst,
            3 => line += r.sleb()?,
            4 => file = r.uleb()?,
            8 => addr += ((255 - opcode_base as u64) / line_range) * min_inst,
            9 => addr += r.n(2)?,
            _ => {
                for _ in 0..std_len.get(op as usize).copied().unwrap_or(0) {
                    r.uleb()?;
                }
            }
        }
    }
    Some(())
}

/// Builds the `.zell_lines` table of executable `output`, adds it and
/// removes the DWARF (unless `keep_debug`) and the symbol table (when
/// `strip`). Returns the table's size. Executables that are not ELF64
/// little-endian are left as they are.
pub fn finish(output: &Path, names: &Names, keep_debug: bool, strip: bool) -> Result<usize, String> {
    let f = std::fs::read(output).map_err(|e| format!("cannot read {}: {e}", output.display()))?;
    if f.len() < 64 || &f[..4] != b"\x7fELF" || f[4] != 2 || f[5] != 1 {
        return Ok(0);
    }
    let h = |off: usize, k: usize| R { b: &f, p: off }.n(k).unwrap_or(0);
    let (phoff, shoff, phentsize, phnum, shentsize, shnum, shstrndx) = (h(0x20, 8), h(0x28, 8), h(0x36, 2), h(0x38, 2), h(0x3a, 2), h(0x3c, 2) as usize, h(0x3e, 2) as usize);
    if shoff == 0 || shentsize != 64 || shnum == 0 || shstrndx >= shnum || shoff as usize + shnum * 64 > f.len() {
        return Ok(0);
    }
    let mut shs: Vec<Sh> = (0..shnum)
        .map(|i| {
            let mut raw = [0u8; 64];
            raw.copy_from_slice(&f[shoff as usize + i * 64..shoff as usize + i * 64 + 64]);
            Sh { name: String::new(), raw }
        })
        .collect();
    let shstr = shs[shstrndx].data(&f).to_vec();
    for s in shs.iter_mut() {
        s.name = cstr_at(&shstr, s.get(0, 4));
    }
    let by_name = |n: &str| shs.iter().find(|s| s.name == n);
    let exec_ranges: Vec<(u64, u64)> = shs.iter().filter(|s| s.flags() & SHF_EXECINSTR != 0 && s.flags() & SHF_ALLOC != 0).map(|s| (s.addr(), s.addr() + s.size())).collect();
    let exec = |a: u64| a != 0 && exec_ranges.iter().any(|&(lo, hi)| a >= lo && a < hi);
    // Lines.
    let mut files = Vec::new();
    let empty: &[u8] = &[];
    let mut rows = match by_name(".debug_line") {
        Some(s) => line_rows(s.data(&f), by_name(".debug_line_str").map_or(empty, |s| s.data(&f)), by_name(".debug_str").map_or(empty, |s| s.data(&f)), names, &mut files, &exec),
        None => Vec::new(),
    };
    rows.sort_by_key(|r| r.0);
    let mut lines: Vec<(u64, u32)> = Vec::new();
    for (a, fi, l) in rows {
        let v = fi << 20 | l;
        if let Some(last) = lines.last_mut() {
            if last.0 == a {
                last.1 = v;
                continue;
            }
        }
        lines.push((a, v));
    }
    lines.dedup_by(|b, a| a.1 == b.1);
    // Functions: the source ranges of the functions in the table's files.
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
        if let Some(fi) = files.iter().position(|f| f == path) {
            funcs.push(((fi as u32) << 20 | (*lo).min(0xfffff), *hi, intern(name, &mut strtab)));
        }
    }
    funcs.sort();
    funcs.dedup();
    let file_offs: Vec<u32> = files.iter().map(|p| intern(p, &mut strtab)).collect();
    let base = lines.first().map_or(0, |l| l.0);
    let top = lines.last().map_or(0, |l| l.0);
    if top - base > u32::MAX as u64 {
        return Err("code larger than 4 GiB: no line table".into());
    }
    let mut tab: Vec<u8> = Vec::new();
    tab.extend_from_slice(b"ZLN1");
    for v in [file_offs.len(), lines.len(), funcs.len(), strtab.len(), 0] {
        tab.extend_from_slice(&(v as u32).to_le_bytes());
    }
    tab.extend_from_slice(&base.to_le_bytes());
    for o in &file_offs {
        tab.extend_from_slice(&o.to_le_bytes());
    }
    for (a, v) in &lines {
        tab.extend_from_slice(&((a - base) as u32).to_le_bytes());
        tab.extend_from_slice(&v.to_le_bytes());
    }
    for (fl, hi, n) in &funcs {
        for v in [fl, hi, n] {
            tab.extend_from_slice(&v.to_le_bytes());
        }
    }
    tab.extend_from_slice(&strtab);
    // Rewrite: keep the loaded image, drop what goes, append the table.
    let symtab_str = shs.iter().find(|s| s.ty() == 2).map(|s| s.link() as usize);
    let drop: Vec<bool> = shs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if i == 0 || s.flags() & SHF_ALLOC != 0 {
                return false;
            }
            s.name == ".zell_lines" || i == shstrndx || (!keep_debug && s.name.starts_with(".debug_")) || (strip && (s.ty() == 2 || Some(i) == symtab_str))
        })
        .collect();
    // Sections that go must come after every kept allocated one.
    let last_alloc = shs.iter().rposition(|s| s.flags() & SHF_ALLOC != 0).unwrap_or(0);
    if drop.iter().take(last_alloc + 1).any(|&d| d) {
        return Err("unexpected section order: no line table".into());
    }
    let mut cut = (phoff + phentsize * phnum) as usize;
    for i in 0..phnum as usize {
        let p = phoff as usize + i * phentsize as usize;
        cut = cut.max((h(p + 8, 8) + h(p + 32, 8)) as usize);
    }
    for s in shs.iter().filter(|s| s.flags() & SHF_ALLOC != 0 && s.ty() != 8) {
        cut = cut.max((s.offset() + s.size()) as usize);
    }
    let mut out = f[..cut.min(f.len())].to_vec();
    let mut newidx = vec![0usize; shnum];
    let mut kept: Vec<Sh> = Vec::new();
    for (i, s) in shs.iter().enumerate() {
        if drop[i] {
            continue;
        }
        newidx[i] = kept.len();
        let mut s = s.clone();
        if s.flags() & SHF_ALLOC == 0 && s.ty() != 8 && s.offset() as usize + s.size() as usize > cut && i != 0 {
            let al = s.get(48, 8).max(1) as usize;
            while out.len() % al != 0 {
                out.push(0);
            }
            let off = out.len() as u64;
            out.extend_from_slice(shs[i].data(&f));
            s.set(24, 8, off);
        }
        kept.push(s);
    }
    for s in kept.iter_mut() {
        let l = s.link() as usize;
        if l != 0 && l < shnum {
            s.set(40, 4, newidx[l] as u64);
        }
        if s.flags() & 0x40 != 0 {
            let inf = s.get(44, 4) as usize;
            if inf < shnum {
                s.set(44, 4, newidx[inf] as u64);
            }
        }
    }
    let add = |name: &str, ty: u32, data: &[u8], out: &mut Vec<u8>, kept: &mut Vec<Sh>| {
        let mut s = Sh { name: name.to_string(), raw: [0u8; 64] };
        while out.len() % 8 != 0 {
            out.push(0);
        }
        s.set(4, 4, ty as u64);
        s.set(24, 8, out.len() as u64);
        s.set(32, 8, data.len() as u64);
        s.set(48, 8, 1);
        out.extend_from_slice(data);
        kept.push(s);
    };
    add(".zell_lines", 1, &tab, &mut out, &mut kept);
    let mut names_tab = vec![0u8];
    for s in kept.iter_mut().skip(1) {
        let o = names_tab.len();
        names_tab.extend_from_slice(s.name.as_bytes());
        names_tab.push(0);
        s.set(0, 4, o as u64);
    }
    let shstr_name = names_tab.len();
    names_tab.extend_from_slice(b".shstrtab\0");
    add(".shstrtab", 3, &names_tab, &mut out, &mut kept);
    kept.last_mut().unwrap().set(0, 4, shstr_name as u64);
    while out.len() % 8 != 0 {
        out.push(0);
    }
    let new_shoff = out.len() as u64;
    for s in &kept {
        out.extend_from_slice(&s.raw);
    }
    out[0x28..0x30].copy_from_slice(&new_shoff.to_le_bytes());
    out[0x3c..0x3e].copy_from_slice(&(kept.len() as u16).to_le_bytes());
    out[0x3e..0x40].copy_from_slice(&((kept.len() - 1) as u16).to_le_bytes());
    let perm = std::fs::metadata(output).map(|m| m.permissions()).ok();
    let tmp = output.with_extension("zell-lines.tmp");
    std::fs::write(&tmp, &out).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    if let Some(p) = perm {
        let _ = std::fs::set_permissions(&tmp, p);
    }
    std::fs::rename(&tmp, output).map_err(|e| format!("cannot write {}: {e}", output.display()))?;
    Ok(tab.len())
}
