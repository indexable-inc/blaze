//! A database for the tests: the build's database, as `Db::for_tests()` makes it. Linking
//! `hir` links hir-ty-mir's MIR table.

use hir as _;

pub(crate) type TestDB = base_db::Db;
