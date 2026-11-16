//! refused: is a secret
// A log record asserted to hold no secret, which holds one (`none::<_, AnySecret>`).
use bstd::{keys::Secret, own::{AnySecret, none}};

pub struct Event {
    pub at: u64,
    pub key: Option<Secret>,
}

const _: () = assert!(none::<Event, AnySecret>());
