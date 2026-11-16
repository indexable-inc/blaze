//! The tests' database: the build's [`Db`](base_db::Db), made by
//! [`Db::for_tests`](base_db::Db::for_tests).

use hir_ty_mir as _;

pub(crate) type TestDB = base_db::Db;
