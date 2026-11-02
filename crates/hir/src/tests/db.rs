//! The tests' database: the build's [`Db`](base_db::Db), made by
//! [`Db::for_tests`](base_db::Db::for_tests). It has no plane, so `run_all` runs a batch on the
//! calling thread in the thread's [`RunOrder`](base_db::RunOrder): a result that changed with
//! the order would depend on the plane's scheduling, which none may.

pub(crate) type TestDB = base_db::Db;
