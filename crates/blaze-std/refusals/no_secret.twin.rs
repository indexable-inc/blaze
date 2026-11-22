//! twin of no_secret.rs: the secret removed, so it compiles.
use bstd::own::{AnySecret, none};

pub struct Event {
    pub at: u64,
}

const _: () = assert!(none::<Event, AnySecret>());
