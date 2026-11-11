//! blaze-client as a Node-API addon (docs/client.md), for TypeScript on Bun and Node 20+.
//!
//! The addon exports three functions, which `index.js` wraps in the `Client` that
//! `index.d.ts` types: `connect(optionsJson)`, a promise of a handle; `call(handle, method,
//! optionsJson, onEvent?)`, a promise of the answer's JSON; and `close(handle)`. Options and
//! answers cross as JSON text, the API's one rendering ([`blaze_client::foreign`]), so the
//! addon touches no JS object but strings, functions, promises and its handle.
//!
//! Each client has one worker thread ([`worker`]), which owns the socket and blocks on it
//! ([`blaze_client::Client::next`]); the JS thread never waits. A call is a command on a
//! channel and a wake of the worker; each response and each end is posted back through the
//! client's threadsafe function and settles its promise on the JS thread ([`js`]). Many calls
//! may be under way on one client.
//!
//! It is written against Node-API's stable C ABI with the functions it needs declared by hand
//! (`js::sys`): no binding crate, so no proc macro and no build script. The symbols are the
//! host's (`node`, `bun`), found when the addon is loaded.
//!
//! What is touched on which thread: a client's state (`js::Js`) only on the JS thread (the
//! exported functions, the threadsafe function's call and the finalizers all run there),
//! behind an `Rc<RefCell>` with one strong count for the handle and one for the threadsafe
//! function; the worker has the channel's receiver, the threadsafe function (callable from
//! any thread) and the client. [`js`] is the one module with unsafe code.

#![deny(unsafe_code)]
#![cfg_attr(target_os = "macos", feature(link_arg_attribute))]

#[expect(unsafe_code, reason = "Node-API's C ABI: raw handles in and out, each call's rule stated")]
pub mod js;
mod worker;
