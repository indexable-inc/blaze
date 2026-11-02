//! The tests' database: base-db's test `Db`, with hir linked for the MIR and display providers.

use hir as _;

pub(crate) type TestDB = base_db::Db;
