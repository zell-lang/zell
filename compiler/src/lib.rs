//! Zell compiler library. Phases:
//! lexer -> parser (AST) -> sema (names, types) -> codegen (LLVM IR).

pub mod ast;
pub mod build;
pub mod cfg;
pub mod codegen;
pub mod crosslink;
pub mod dap;
pub mod datacli;
pub mod diagnostics;
pub mod doc;
pub mod driver;
pub mod fmt;
pub mod fxhash;
pub mod hir;
pub mod lexer;
pub mod lines;
pub mod package;
pub mod parcodegen;
pub mod parser;
pub mod paths;
pub mod sema;
pub mod session;
pub mod span;
pub mod stdindex;
pub mod target;
pub mod token;
pub mod toolchain;
pub mod types;
