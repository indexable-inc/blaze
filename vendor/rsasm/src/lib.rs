//! rsasm — a multi-syntax, multi-architecture assembler.
//!
//! ```
//! # // Hidden: the example needs a backend, and each one is a cargo feature.
//! # #[cfg(feature = "x86")] {
//! use rsasm::{arch, assembler::{Assembler, Options}, section::SectionId};
//!
//! let mut asm = Assembler::new(arch::lookup("x86-64").unwrap(), Options::new());
//! asm.assemble_str("example.s", "movq %rbx, %rax\nret\n");
//! assert!(asm.finish());
//! assert_eq!(asm.section_bytes(SectionId(0)), vec![0x48, 0x89, 0xd8, 0xc3]);
//! # }
//! ```
//!
//! The pipeline is [`lexer`] to a parser to an architecture backend
//! ([`arch`]), which produces [`section`] fragments that the layout resolves
//! into bytes and relocations for [`output`]. With the `link` feature there
//! is one more stage after that, which hands the object to the qld linker
//! and gets a program back.
//!
//! # What is public API
//!
//! rsasm is an assembler first and a library second, and almost all of it is
//! an implementation detail: the opcode tables, the operand parsers, the
//! expression arena, the macro engine and the layout are free to change in
//! any release. Only the following is API, and only it is documented here:
//!
//! * [`assembler::Assembler`], and the methods that drive it —
//!   [`new`](assembler::Assembler::new),
//!   [`assemble_str`](assembler::Assembler::assemble_str),
//!   [`assemble_path`](assembler::Assembler::assemble_path),
//!   [`assemble_file`](assembler::Assembler::assemble_file),
//!   [`assemble_prelude`](assembler::Assembler::assemble_prelude),
//!   [`finish`](assembler::Assembler::finish),
//!   [`section_bytes`](assembler::Assembler::section_bytes),
//!   [`diags`](assembler::Assembler::diags),
//!   [`source_map`](assembler::Assembler::source_map),
//!   [`options`](assembler::Assembler::options) and
//!   [`target`](assembler::Assembler::target).
//! * [`assembler::Options`], built with
//!   [`Options::new`](assembler::Options::new) and its `with_*` methods, the
//!   values those take ([`assembler::ImplicitIt`]), and [`output::Format`].
//! * [`arch::lookup`], [`arch::available`], [`arch::default_arch`], and the
//!   [`arch::Architecture`] trait together with the types its methods take
//!   and return. Backends live inside this crate, but the trait is the
//!   documented seam between the core and one.
//! * [`section::SectionId`] and the section description the trait exposes.
//! * The object writers: [`output::elf::build`], [`output::coff::build`],
//!   [`output::macho::build`], [`output::raw::build`] and
//!   [`output::ihex::build`].
// The `link` bullet names items that only exist behind the feature, so it
// is written as a conditional doc attribute rather than as `//!` lines: a
// `cargo doc --no-default-features` would otherwise report links to a
// module that is not there.
#![cfg_attr(
    feature = "link",
    doc = "* With the `link` feature, which is in the default set:",
    doc = "  [`link::link`], [`link::supports`], [`link::Options`] with its",
    doc = "  `with_*` builders, [`link::Kind`] and [`link::Error`]. Without",
    doc = "  the feature the crate has no dependencies at all and the module",
    doc = "  does not exist."
)]
//! * Diagnostics: [`diag::DiagBag`], [`diag::Diagnostic`],
//!   [`diag::Severity`] and [`diag::DiagBag::render`], with
//!   [`source::SourceMap`] and [`source::Span`] as far as rendering needs
//!   them.
//! * [`lexer::Dialect`].
//! * The symbol table as an object writer and a test describe it:
//!   [`symbol::Binding`], [`symbol::SymType`], [`symbol::Visibility`] and
//!   [`symbol::SymbolValue`].
//!
//! Anything reachable but marked `Not API.`, and anything hidden from these
//! docs, is not covered: it exists because the crate is split into modules,
//! or because rsasm's own integration tests are separate crates that have to
//! reach it. The public types that will keep growing are `#[non_exhaustive]`,
//! so build [`assembler::Options`] with its constructors rather than with a
//! struct literal.

pub mod arch;
pub mod assembler;
pub(crate) mod codeview;
pub(crate) mod coff;
pub(crate) mod cursor;
pub mod diag;
pub(crate) mod dialect;
pub(crate) mod dialect_cc;
pub(crate) mod directives;
pub(crate) mod dwarf;
pub(crate) mod expr;
pub(crate) mod intern;
pub(crate) mod layout;
pub mod lexer;
#[cfg(feature = "link")]
pub mod link;
pub(crate) mod literals;
pub(crate) mod macros;
pub(crate) mod mapping;
pub(crate) mod nasm;
pub mod output;
pub(crate) mod parser;
pub(crate) mod reloc;
pub mod section;
pub mod source;
pub mod symbol;
