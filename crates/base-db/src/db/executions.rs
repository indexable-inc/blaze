//! How often each query ran, and the first to start, while `RUSTD_TRACE` is set: a database
//! made then reports every execution here, and the build prints what [`take`] and [`order`]
//! say after each request.

use parking_lot::Mutex;

use blaze_hash::HashMap;
use blaze_query::db::{Database as _, Event, EventKind};

use super::Db;

/// How many of the first queries to start [`order`] names.
const ORDER_KEPT: usize = 300;

/// By ingredient index ([`blaze_query::db::Storage::ingredient_name`] names one).
static RAN: Mutex<Option<HashMap<u32, u32>>> = Mutex::new(None);
static ORDER: Mutex<Vec<u32>> = Mutex::new(Vec::new()); // alloc: empty until traced

/// Whether a database made now counts its executions.
pub(super) fn traced() -> bool {
    std::env::var_os("RUSTD_TRACE").is_some()
}

pub(super) fn record(event: &Event) {
    let EventKind::WillExecute { database_key } = event.kind else { return };
    let query = database_key.ingredient_index();
    let mut ran = RAN.lock();
    *ran.get_or_insert_default().entry(query).or_default() += 1;
    let mut order = ORDER.lock();
    if order.len() < ORDER_KEPT {
        order.push(query);
    }
}

/// The first queries that started since the last call, in the order they started.
pub fn order(db: &Db) -> Vec<String> {
    let order = std::mem::take(&mut *ORDER.lock());
    // alloc: RUSTD_TRACE's report, once per request and only when traced
    order.into_iter().map(|query| db.storage().ingredient_name(query).to_owned()).collect()
}

/// The queries that ran since the last call, most often first.
pub fn take(db: &Db) -> Vec<(String, u32)> {
    let ran = RAN.lock().take().unwrap_or_default();
    let mut ran: Vec<_> = ran
        .into_iter()
        .map(|(query, count)| (db.storage().ingredient_name(query).to_owned(), count))
        .collect(); // alloc: RUSTD_TRACE's report, once per request and only when traced
    ran.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ran
}
