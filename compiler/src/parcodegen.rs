//! Parallel object emission for large modules (compile time, see
//! benchmarks/README.md "Compile time").
//!
//! After optimisation the module is cut into `k` partitions that LLVM
//! compiles to machine code on `k` threads, and the partial objects are
//! merged into one relocatable object (`clang -r`), so the rest of the build
//! (object cache, link) is unchanged. Optimisation (inlining) has already
//! seen the whole module, so splitting loses nothing at run time.
//!
//! How: every internal/private definition is renamed to a name unique to
//! the output object and made `hidden` external, the module is serialised
//! to bitcode, and each thread parses it into its own context and turns the
//! definitions it does not own into declarations. Functions are spread over
//! the partitions by size (greedy, least loaded first); global variables and
//! the `llvm.*` arrays stay in partition 0. Only for ELF hosts with a clang
//! driver; anything unexpected falls back to the serial path.

use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::module::{Linkage, Module};
use inkwell::targets::FileType;
use inkwell::llvm_sys::prelude::LLVMValueRef;
use inkwell::values::AsValueRef;
use inkwell::{GlobalVisibility, OptimizationLevel};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Below this many instructions a module is emitted serially (the split
/// costs every thread a parse of the whole module: about 30% more CPU in
/// all), and each partition gets at least about this many.
const MIN_INSTRUCTIONS: usize = 60_000;
const PER_PARTITION: usize = 40_000;

/// Number of code generation threads (`ZELL_CODEGEN_THREADS`, default:
/// the available parallelism, at most 8; 1 disables the split).
fn threads() -> usize {
    if let Some(n) = std::env::var("ZELL_CODEGEN_THREADS").ok().and_then(|v| v.parse::<usize>().ok()) {
        return n.clamp(1, 64);
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)
}

fn instructions(f: inkwell::values::FunctionValue) -> usize {
    let mut n = 0;
    let mut bb = f.get_first_basic_block();
    while let Some(b) = bb {
        let mut i = b.get_first_instruction();
        while let Some(ins) = i {
            n += 1;
            i = ins.get_next_instruction();
        }
        bb = b.get_next_basic_block();
    }
    n
}

/// Writes `module` (optimised) to the object file `path` using several
/// threads. Returns `Ok(false)` when the module is too small or the target
/// does not allow it (the caller then writes it serially).
pub fn write_object_parallel(module: &Module, opt: OptimizationLevel, path: &Path, clang: impl FnOnce() -> Option<String>) -> Result<bool, String> {
    let k = threads();
    let target = crate::target::current();
    if k < 2 || !target.is_host() || target.object_format() != crate::target::ObjectFormat::Elf {
        return Ok(false);
    }
    // Cheap pre-check (blocks rarely hold more than 12 instructions on average).
    let blocks: usize = module.get_functions().map(|f| f.count_basic_blocks() as usize).sum();
    if blocks * 12 < MIN_INSTRUCTIONS {
        return Ok(false);
    }
    // Weights of the defined functions, in module order (which bitcode
    // preserves, so partitions refer to functions by this index).
    let weights: Vec<usize> = module.get_functions().filter(|f| f.count_basic_blocks() > 0).map(|f| instructions(f) + 8).collect();
    let total: usize = weights.iter().sum();
    if std::env::var_os("ZELL_CODEGEN_DEBUG").is_some() {
        eprintln!("zell: parcodegen: {} functions, {} instructions", weights.len(), total);
    }
    if total < MIN_INSTRUCTIONS || weights.len() < 2 * k {
        return Ok(false);
    }
    let k = k.min(1 + total / PER_PARTITION);
    let Some(clang) = clang() else { return Ok(false) };
    // Unique names for the symbols that become visible across partitions.
    let tag = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(path.to_string_lossy().as_bytes());
        h.update(std::process::id().to_le_bytes());
        crate::package::lock::hex(&h.finalize()[..6])
    };
    let promote = |gv: inkwell::values::GlobalValue| {
        if gv.is_declaration() {
            return;
        }
        if matches!(gv.get_linkage(), Linkage::Internal | Linkage::Private) {
            let n = gv.get_name().to_string_lossy().into_owned();
            gv.set_name(&format!("__slp{tag}.{n}"));
            gv.set_linkage(Linkage::External);
            gv.set_visibility(GlobalVisibility::Hidden);
        }
    };
    for f in module.get_functions() {
        promote(f.as_global_value());
    }
    for g in module.get_globals() {
        if !g.get_name().to_string_lossy().starts_with("llvm.") {
            promote(g);
        }
    }
    // Greedy assignment, heaviest first.
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|a, b| weights[*b].cmp(&weights[*a]).then(a.cmp(b)));
    let mut load = vec![0usize; k];
    let mut assign = vec![0usize; weights.len()];
    for i in order {
        let p = (0..k).min_by_key(|j| (load[*j], *j)).unwrap();
        load[p] += weights[i];
        assign[i] = p;
    }
    // An outlined unwind cleanup (codegen/unwind_lower.rs, attribute
    // "zell-unwind-cleanup") recovers its frame's slots through labels
    // local to its function's object: same partition. The function is the
    // first operand of its `llvm.localrecover` calls.
    let defined: Vec<inkwell::values::FunctionValue> = module.get_functions().filter(|f| f.count_basic_blocks() > 0).collect();
    let index: std::collections::HashMap<LLVMValueRef, usize> = defined.iter().enumerate().map(|(i, f)| (f.as_value_ref(), i)).collect();
    for (i, f) in defined.iter().enumerate() {
        if f.get_string_attribute(inkwell::attributes::AttributeLoc::Function, "zell-unwind-cleanup").is_none() {
            continue;
        }
        if let Some(parent) = crate::codegen::unwind_parent(*f).and_then(|p| index.get(&p)) {
            assign[i] = assign[*parent];
        }
    }
    let bitcode = Arc::new(module.write_bitcode_to_memory().as_slice().to_vec());
    let assign = Arc::new(assign);
    let parts: Vec<PathBuf> = (0..k).map(|i| path.with_extension(format!("part{i}.o"))).collect();
    let handles: Vec<_> = (0..k)
        .map(|i| {
            let bitcode = Arc::clone(&bitcode);
            let assign = Arc::clone(&assign);
            let out = parts[i].clone();
            std::thread::spawn(move || emit_partition(&bitcode, &assign, i, opt, &out))
        })
        .collect();
    let mut err = None;
    for h in handles {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => err = Some(e),
            Err(_) => err = Some("code generation thread panicked".to_string()),
        }
    }
    let cleanup = |parts: &[PathBuf]| {
        for p in parts {
            let _ = std::fs::remove_file(p);
        }
    };
    if let Some(e) = err {
        cleanup(&parts);
        return Err(e);
    }
    let out = std::process::Command::new(&clang).arg("-r").arg("-nostdlib").arg("-o").arg(path).args(&parts).output();
    cleanup(&parts);
    match out {
        Ok(o) if o.status.success() => Ok(true),
        Ok(o) => Err(format!("merging partial objects failed:\n{}", String::from_utf8_lossy(&o.stderr))),
        Err(e) => Err(format!("cannot run {clang}: {e}")),
    }
}

/// One partition: parse, drop the definitions owned by other partitions,
/// generate code.
fn emit_partition(bitcode: &[u8], assign: &[usize], me: usize, opt: OptimizationLevel, out: &Path) -> Result<(), String> {
    let ctx = Context::create();
    let buf = MemoryBuffer::create_from_memory_range_copy(bitcode, "part");
    let module = Module::parse_bitcode_from_buffer(&buf, &ctx).map_err(|e| format!("internal compiler error: cannot reparse the module: {}", e.to_string()))?;
    let mut idx = 0;
    for f in module.get_functions() {
        if f.count_basic_blocks() == 0 {
            continue;
        }
        let mine = assign.get(idx).copied().unwrap_or(0) == me;
        idx += 1;
        if mine {
            continue;
        }
        while let Some(b) = f.get_first_basic_block() {
            // SAFETY: the block belongs to f, which is being turned into a declaration.
            unsafe { b.delete() }.map_err(|_| "internal compiler error: cannot drop a function body".to_string())?;
        }
        let v = f.as_value_ref();
        // SAFETY: plain attribute changes on a function of this module.
        unsafe {
            llvm_sys::comdat::LLVMSetComdat(v, std::ptr::null_mut());
            llvm_sys::core::LLVMSetSection(v, std::ptr::null());
            llvm_sys::core::LLVMGlobalClearMetadata(v);
            if llvm_sys::core::LLVMHasPersonalityFn(v) != 0 {
                llvm_sys::core::LLVMSetPersonalityFn(v, std::ptr::null_mut());
            }
        }
        f.set_linkage(Linkage::External);
    }
    if me != 0 {
        let globals: Vec<_> = module.get_globals().collect();
        for g in globals {
            if g.is_declaration() {
                continue;
            }
            let v = g.as_value_ref();
            if g.get_name().to_string_lossy().starts_with("llvm.") {
                // SAFETY: appending arrays (llvm.used, ctors) live in partition 0 only.
                unsafe { llvm_sys::core::LLVMDeleteGlobal(v) };
                continue;
            }
            // SAFETY: a null initializer turns the definition into a declaration.
            unsafe {
                llvm_sys::core::LLVMSetInitializer(v, std::ptr::null_mut());
                llvm_sys::comdat::LLVMSetComdat(v, std::ptr::null_mut());
                llvm_sys::core::LLVMSetSection(v, std::ptr::null());
            }
            g.set_linkage(Linkage::External);
        }
    }
    let machine = crate::codegen::host_machine(opt)?;
    machine.write_to_file(&module, FileType::Object, out).map_err(|e| format!("cannot write {}: {}", out.display(), e.to_string()))
}
