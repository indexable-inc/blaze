//! Addresses as types (docs/v2/plan/design/std.md, "Names are typed, and authority is
//! stored"): a [`Scheme`], an [`Origin`] (a scheme, a host and a port: what
//! [`crate::requests::Http::only`] narrows to) and a [`Url`], each checked as it is made. A `Url`
//! of any scheme is what [`crate::opener::Opener`] hands to the user's default application; one
//! with an authority also has its [`Url::origin`] and its [`Url::target`], which is what an
//! HTTP request needs.
//!
//! The types are blaze-url's, their one home (a leaf `no_std` crate that blaze-http's client
//! asks by too), here under bstd's names.

pub use blaze_url::url::{Origin, Scheme, Url, UrlError, UrlWhy};
