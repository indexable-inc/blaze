//! The proxies the process's environment names, read once as a client is made: not part of the
//! core, which is handed them in its [`crate::Config`]. Which variables, and how they are read,
//! is [`crate::proxy`]'s documentation.

use crate::proxy::{Proxies, ProxyError};

/// The proxies this process's environment names; an error names the variable that can't be
/// used.
#[allow(
    clippy::disallowed_methods,
    reason = "the proxy variables curl and cargo read, read once where a client is made and handed to it typed"
)]
pub fn from_env() -> Result<Proxies, ProxyError> {
    Proxies::from_vars(|name| std::env::var_os(name), cfg!(windows))
}
