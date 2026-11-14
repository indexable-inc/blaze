//! blaze's typed SQL, the checker (docs/v2/plan/design/sql.md).
//!
//! One statement of a SQL subset goes through four pure steps, each its own module:
//!
//! 1. [`parse`]: text to an [`ast::Statement`]. One statement, named parameters only (`$name`).
//! 2. [`infer`]: the statement against a [`schema::Schema`], to a [`desc::Description`]: every
//!    parameter's name, type and nullability, and what the statement returns.
//! 3. [`render`]: the statement as an engine's text ([`desc::Backend`]), with that engine's
//!    placeholders and what binds to each.
//! 4. [`ddl`]: `create table` text for a [`schema::Table`].
//!
//! [`check`] is steps 1 and 2 and the only maker of a [`Checked`] statement, which is all a
//! driver's `prepare` takes: text decided at run time has no way to an engine.
//!
//! The crate owns no IO, clock or thread, and allocates only in the allocator it is handed, so
//! the compiler runs it in const evaluation and a test runs it as it is.

// Under test the crate has std, which its tests and blaze-sim's checks run on.
#![cfg_attr(not(test), no_std)]
#![feature(allocator_ext)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

extern crate alloc;

pub mod ast;
pub mod ddl;
pub mod desc;
pub mod error;
pub mod funcs;
pub mod infer;
mod lex;
pub mod parse;
pub mod print;
pub mod render;
pub mod same;
pub mod schema;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;

pub use crate::error::Error;

/// A statement that parsed and typed against a schema. Only [`check`] makes one.
#[derive(Debug)]
pub struct Checked<'s, A: Allocator> {
    schema: schema::Schema<'s>,
    statement: ast::Statement<'s, A>,
    description: desc::Description<'s, A>,
}

impl<'s, A: Allocator> Checked<'s, A> {
    /// The schema it was checked against.
    pub fn schema(&self) -> &schema::Schema<'s> {
        &self.schema
    }

    /// The statement as parsed.
    pub fn statement(&self) -> &ast::Statement<'s, A> {
        &self.statement
    }

    /// Its parameters and what it returns.
    pub fn description(&self) -> &desc::Description<'s, A> {
        &self.description
    }
}

/// Parses `text` as one statement and types it against `schema`.
///
/// `text` is a constant of the program: the typed surface passes a const generic argument, and
/// `blaze::sql_text_at_run_time` refuses any other caller's text that is not one.
pub fn check<'s, A: Allocator + Copy>(
    schema: &schema::Schema<'s>,
    text: &'s str,
    alloc: A,
) -> Result<Checked<'s, A>, Error<'s>> {
    let statement = parse::parse(text, alloc).map_err(Error::Syntax)?;
    let description = infer::infer(schema, &statement, alloc).map_err(Error::Type)?;
    Ok(Checked { schema: *schema, statement, description })
}

/// [`check`] with a planted bug in inference, for the controls of tests that hold inference to
/// an engine (`the_oracle_catches_..`). Never called by shipping code.
#[cfg(feature = "plant")]
pub fn check_planted<'s, A: Allocator + Copy>(
    schema: &schema::Schema<'s>,
    text: &'s str,
    plant: infer::Plant,
    alloc: A,
) -> Result<Checked<'s, A>, Error<'s>> {
    let statement = parse::parse(text, alloc).map_err(Error::Syntax)?;
    let description =
        infer::infer_planted(schema, &statement, plant, alloc).map_err(Error::Type)?;
    Ok(Checked { schema: *schema, statement, description })
}
