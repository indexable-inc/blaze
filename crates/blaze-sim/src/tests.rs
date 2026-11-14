//! The layers the driver is built on, proved with the driver: the tests of [`crate::tape`],
//! [`crate::text`] and [`crate::search`] run through [`crate::check`](fn@crate::check) and
//! [`crate::control`](fn@crate::control), which are built on those layers. They sit here, above
//! every module, so that no module imports from one built on it.

mod search;
mod tape;
mod text;
