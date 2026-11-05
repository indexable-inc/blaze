//! The tests' database: base-db's test `Db`, with hir-ty-mir linked for the MIR providers (a
//! constant in a pattern is evaluated).

use hir_ty_mir as _;

pub(crate) type TestDB = base_db::Db;
