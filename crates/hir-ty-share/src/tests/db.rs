//! The tests' database: base-db's test `Db`, with hir-ty-mir linked for the MIR providers.

use hir_ty_mir as _;

pub(crate) type TestDB = base_db::Db;
