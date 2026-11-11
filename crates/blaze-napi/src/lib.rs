//! blaze-client as a Node-API addon (docs/client.md), for TypeScript on Bun and Node 20+.
//!
//! The addon exports three functions, which `index.js` wraps in the `Client` that
//! `index.d.ts` types: `connect(optionsJson)`, a promise of a handle; `call(handle, method,
//! optionsJson, onEvent?)`, a promise of the answer's JSON; and `close(handle)`. Options and
//! answers cross as JSON text, the API's one rendering ([`blaze_client::foreign`]), so the
//! addon touches no JS object but strings, functions, promises and its handle.
//!
//! Each client has one worker thread, which owns the socket and blocks on it
//! ([`blaze_client::Client::next`]); the JS thread never waits. A call is a command on a
//! channel and a wake of the worker; each response and each end is posted back through the
//! client's threadsafe function and settles its promise on the JS thread. Many calls may be
//! under way on one client.
//!
//! It is written against Node-API's stable C ABI with the functions it needs declared by hand
//! ([`sys`]): no binding crate, so no proc macro and no build script. The symbols are the
//! host's (`node`, `bun`), found when the addon is loaded.
//!
//! What is touched on which thread: [`Js`] only on the JS thread (the exported functions, the
//! threadsafe function's call and the finalizers all run there), behind an `Rc<RefCell>` with
//! one strong count for the handle and one for the threadsafe function; the worker has the
//! channel's receiver, the threadsafe function (callable from any thread) and the client.

#![expect(unsafe_code, reason = "Node-API's C ABI: raw handles in and out, each call's rule stated")]
#![cfg_attr(target_os = "macos", feature(link_arg_attribute))]

use std::{
    cell::RefCell,
    ffi::{c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{null, null_mut},
    rc::Rc,
    time::Duration,
};

use blaze_api::{Envelope, json};
use blaze_client::{
    Error, Fault, Next, Waker,
    foreign::{self, Calls, Told},
};
use crossbeam_channel::{Receiver, Sender, TryRecvError};

/// Node-API, as `node_api.h` and `js_native_api.h` declare what this addon calls.
mod sys {
    use std::ffi::{c_char, c_void};

    pub(crate) type Env = *mut c_void;
    pub(crate) type Value = *mut c_void;
    pub(crate) type Deferred = *mut c_void;
    pub(crate) type Ref = *mut c_void;
    pub(crate) type CallbackInfo = *mut c_void;
    pub(crate) type Tsfn = *mut c_void;
    pub(crate) type Status = i32;

    pub(crate) const OK: Status = 0;
    /// `napi_valuetype`'s `napi_function`.
    pub(crate) const FUNCTION: i32 = 7;
    /// `napi_tsfn_nonblocking` and `napi_tsfn_release`.
    pub(crate) const NONBLOCKING: i32 = 0;
    pub(crate) const RELEASE: i32 = 0;

    pub(crate) type Callback = unsafe extern "C" fn(env: Env, info: CallbackInfo) -> Value;
    pub(crate) type Finalize = unsafe extern "C" fn(env: Env, data: *mut c_void, hint: *mut c_void);
    pub(crate) type CallJs =
        unsafe extern "C" fn(env: Env, callback: Value, context: *mut c_void, data: *mut c_void);

    // The host's symbols, left undefined in the addon and bound when it is loaded: macOS's
    // linker is told so; ELF shared objects allow it as they are.
    #[cfg_attr(target_os = "macos", link(kind = "link-arg", name = "-Wl,-undefined,dynamic_lookup"))]
    unsafe extern "C" {
        pub(crate) fn napi_create_function(
            env: Env,
            name: *const c_char,
            length: usize,
            callback: Callback,
            data: *mut c_void,
            result: *mut Value,
        ) -> Status;
        pub(crate) fn napi_set_named_property(
            env: Env,
            object: Value,
            name: *const c_char,
            value: Value,
        ) -> Status;
        pub(crate) fn napi_get_cb_info(
            env: Env,
            info: CallbackInfo,
            argc: *mut usize,
            argv: *mut Value,
            this: *mut Value,
            data: *mut *mut c_void,
        ) -> Status;
        pub(crate) fn napi_get_value_string_utf8(
            env: Env,
            value: Value,
            buffer: *mut c_char,
            size: usize,
            written: *mut usize,
        ) -> Status;
        pub(crate) fn napi_create_string_utf8(
            env: Env,
            text: *const c_char,
            length: usize,
            result: *mut Value,
        ) -> Status;
        pub(crate) fn napi_create_uint32(env: Env, value: u32, result: *mut Value) -> Status;
        pub(crate) fn napi_typeof(env: Env, value: Value, result: *mut i32) -> Status;
        pub(crate) fn napi_get_undefined(env: Env, result: *mut Value) -> Status;
        pub(crate) fn napi_create_promise(
            env: Env,
            deferred: *mut Deferred,
            promise: *mut Value,
        ) -> Status;
        pub(crate) fn napi_resolve_deferred(env: Env, deferred: Deferred, value: Value) -> Status;
        pub(crate) fn napi_reject_deferred(env: Env, deferred: Deferred, value: Value) -> Status;
        pub(crate) fn napi_create_error(
            env: Env,
            code: Value,
            message: Value,
            result: *mut Value,
        ) -> Status;
        pub(crate) fn napi_throw_error(env: Env, code: *const c_char, message: *const c_char) -> Status;
        pub(crate) fn napi_create_external(
            env: Env,
            data: *mut c_void,
            finalize: Option<Finalize>,
            hint: *mut c_void,
            result: *mut Value,
        ) -> Status;
        pub(crate) fn napi_get_value_external(env: Env, value: Value, result: *mut *mut c_void) -> Status;
        pub(crate) fn napi_create_reference(
            env: Env,
            value: Value,
            count: u32,
            result: *mut Ref,
        ) -> Status;
        pub(crate) fn napi_delete_reference(env: Env, reference: Ref) -> Status;
        pub(crate) fn napi_get_reference_value(env: Env, reference: Ref, result: *mut Value) -> Status;
        pub(crate) fn napi_call_function(
            env: Env,
            this: Value,
            function: Value,
            argc: usize,
            argv: *const Value,
            result: *mut Value,
        ) -> Status;
        pub(crate) fn napi_create_threadsafe_function(
            env: Env,
            function: Value,
            async_resource: Value,
            async_resource_name: Value,
            max_queue_size: usize,
            initial_thread_count: usize,
            finalize_data: *mut c_void,
            finalize: Option<Finalize>,
            context: *mut c_void,
            call_js: Option<CallJs>,
            result: *mut Tsfn,
        ) -> Status;
        pub(crate) fn napi_call_threadsafe_function(function: Tsfn, data: *mut c_void, mode: i32) -> Status;
        pub(crate) fn napi_release_threadsafe_function(function: Tsfn, mode: i32) -> Status;
        pub(crate) fn napi_ref_threadsafe_function(env: Env, function: Tsfn) -> Status;
        pub(crate) fn napi_unref_threadsafe_function(env: Env, function: Tsfn) -> Status;
    }
}

/// What the JS thread tells a client's worker.
enum Command {
    Call { call: u64, method: String, options: String },
    Close,
}

/// What a client's worker tells the JS thread.
enum Post {
    Connected(Result<Waker, Error>),
    /// A response of call `call`, as one line of the API's JSON.
    Event { call: u64, json: String },
    /// Call `call` is over: its answer's JSON.
    Ended { call: u64, result: Result<String, Error> },
    /// The worker is gone, nothing more comes: `why` when it panicked.
    Exited { why: Option<String> },
}

/// A call whose promise is not settled.
struct Pending {
    call: u64,
    deferred: sys::Deferred,
    /// The call's `onEvent`, referenced; null for none.
    on_event: sys::Ref,
}

/// One client, as the JS thread holds it.
struct Js {
    tsfn: sys::Tsfn,
    commands: Sender<Command>,
    /// The worker's waker, once it has connected.
    waker: Option<Waker>,
    /// The connect's promise, until it settles.
    connecting: Option<sys::Deferred>,
    next_call: u64,
    pending: Vec<Pending>,
    /// The promises of `close()`, settled when the worker is gone.
    closing: Vec<sys::Deferred>,
    /// The worker is gone.
    closed: bool,
    /// Whether the threadsafe function holds the event loop open: while a promise is unsettled.
    held: bool,
}

type Shared = RefCell<Js>;

/// The threadsafe function, for the worker: Node-API lets any thread call one.
struct Poster(sys::Tsfn);

// SAFETY: a threadsafe function is made to be called from other threads than the one that
// made it; the worker calls only `napi_call_threadsafe_function` on it, and stops before the
// JS thread releases it (the release follows the worker's last post, `Post::Exited`).
unsafe impl Send for Poster {}

impl Poster {
    fn post(&self, post: Post) {
        // alloc: one box a post, freed by the JS thread's `told`.
        let data = Box::into_raw(Box::new(post));
        // SAFETY: the function is live until the JS thread takes `Post::Exited`, which is the
        // last post; `data` is a box the callee takes.
        let status =
            unsafe { sys::napi_call_threadsafe_function(self.0, data.cast(), sys::NONBLOCKING) };
        if status != sys::OK {
            // The environment is tearing down: nobody takes the post.
            // SAFETY: the call did not take `data`, which is the box made above.
            drop(unsafe { Box::from_raw(data) });
        }
    }
}

fn said(panic: Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|it| (*it).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

/// A client's worker thread: connects, then turns commands into requests and events into
/// posts until it is told to close.
fn work(poster: Poster, commands: Receiver<Command>, options: String) {
    let ran = catch_unwind(AssertUnwindSafe(|| run(&poster, &commands, &options)));
    poster.post(Post::Exited { why: ran.err().map(said) });
}

fn run(poster: &Poster, commands: &Receiver<Command>, options: &str) {
    let mut client = match foreign::connect(options.as_bytes()) {
        Ok(client) => client,
        Err(error) => return poster.post(Post::Connected(Err(error))),
    };
    match client.waker() {
        Ok(waker) => poster.post(Post::Connected(Ok(waker))),
        Err(error) => {
            let what = format!("making the client's waker: {error}");
            return poster.post(Post::Connected(Err(Error::Fault(Fault::Io { what }))));
        }
    }
    let mut calls = Calls::new();
    // Each request under way and the call it is.
    let mut ids: Vec<(u64, u64)> = Vec::new();
    let (mut over, mut closing) = (false, false);
    // Bounded by its commands: it ends at a close, and waits only where a wake or the
    // daemon's bytes end the wait.
    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Call { call, method, options }) => {
                    match calls.begin(&mut client, &method, options.as_bytes()) {
                        Ok(id) => ids.push((id, call)),
                        Err(error) => poster.post(Post::Ended { call, result: Err(error) }),
                    }
                }
                Ok(Command::Close) | Err(TryRecvError::Disconnected) => {
                    closing = true;
                    client.shut();
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if over {
            if closing || client.idle(Duration::MAX).is_err() {
                return;
            }
            continue;
        }
        match client.next(Duration::MAX) {
            Ok(Next::Event(event)) => match calls.on(event) {
                Told::Response { id, response } => {
                    if let Some(&(_, call)) = ids.iter().find(|(it, _)| *it == id) {
                        let json = json::response(&Envelope::new(id, response));
                        poster.post(Post::Event { call, json });
                    }
                }
                Told::Ended { id, result } => {
                    if let Some(at) = ids.iter().position(|(it, _)| *it == id) {
                        let (_, call) = ids.swap_remove(at);
                        poster.post(Post::Ended { call, result: result.map(|it| it.json()) });
                    }
                }
                Told::Other(_) => {}
            },
            Ok(Next::Woken | Next::Deadline) => {}
            // The connection is over, and every call under way was told its end before this.
            Err(_) => over = true,
        }
    }
}

/// A Node-API call that failed, on the JS thread.
fn checked(status: sys::Status, what: &str) -> Result<(), String> {
    match status {
        sys::OK => Ok(()),
        status => Err(format!("blaze-napi: {what} failed with napi_status {status}")),
    }
}

/// A JS string of `text`.
///
/// # Safety
///
/// `env` is the environment of the JS thread this runs on.
unsafe fn string(env: sys::Env, text: &str) -> Result<sys::Value, String> {
    let mut value = null_mut();
    // SAFETY: `text` is readable for its length, which is given; `value` is written.
    let status = unsafe { sys::napi_create_string_utf8(env, text.as_ptr().cast(), text.len(), &raw mut value) };
    checked(status, "napi_create_string_utf8").map(|()| value)
}

/// The text of the JS string `value`.
///
/// # Safety
///
/// `env` is the environment of the JS thread this runs on, and `value` a value of it.
unsafe fn text(env: sys::Env, value: sys::Value, what: &str) -> Result<String, String> {
    let mut len = 0;
    // SAFETY: a null buffer asks for the length alone, written to `len`.
    let status = unsafe { sys::napi_get_value_string_utf8(env, value, null_mut(), 0, &raw mut len) };
    checked(status, "napi_get_value_string_utf8").map_err(|_| format!("blaze-napi: {what} must be a string"))?;
    let mut bytes = vec![0u8; len + 1];
    let mut written = 0;
    // SAFETY: the buffer holds `len + 1` bytes, the text and the NUL the call writes.
    let status = unsafe {
        sys::napi_get_value_string_utf8(env, value, bytes.as_mut_ptr().cast(), bytes.len(), &raw mut written)
    };
    checked(status, "napi_get_value_string_utf8")?;
    bytes.truncate(written);
    String::from_utf8(bytes).map_err(|_| format!("blaze-napi: {what} is not UTF-8"))
}

/// A JS `Error` of `message` whose `code` is `code` ([`Error::code`]; 13 for a panic).
///
/// # Safety
///
/// `env` is the environment of the JS thread this runs on.
unsafe fn error(env: sys::Env, code: u32, message: &str) -> Result<sys::Value, String> {
    let (mut made, mut number) = (null_mut(), null_mut());
    // SAFETY: `env` is this thread's; each out pointer is a local written by its call, and the
    // property's name is a NUL-terminated literal.
    unsafe {
        let message = string(env, message)?;
        checked(sys::napi_create_error(env, null_mut(), message, &raw mut made), "napi_create_error")?;
        checked(sys::napi_create_uint32(env, code, &raw mut number), "napi_create_uint32")?;
        checked(sys::napi_set_named_property(env, made, c"code".as_ptr(), number), "napi_set_named_property")?;
    }
    Ok(made)
}

/// Settles `deferred` with `result`: a string, `undefined` for none, or an error.
///
/// # Safety
///
/// `env` is the environment of the JS thread this runs on, and `deferred` a promise's of it,
/// not settled before.
unsafe fn settle(env: sys::Env, deferred: sys::Deferred, result: Result<Option<&str>, (u32, String)>) {
    // SAFETY: `env` is this thread's and `deferred` unsettled, by this function's contract.
    let settled = unsafe {
        match result {
            Ok(Some(answer)) => string(env, answer)
                .and_then(|it| checked(sys::napi_resolve_deferred(env, deferred, it), "napi_resolve_deferred")),
            Ok(None) => {
                let mut undefined = null_mut();
                checked(sys::napi_get_undefined(env, &raw mut undefined), "napi_get_undefined")
                    .and_then(|()| checked(sys::napi_resolve_deferred(env, deferred, undefined), "napi_resolve_deferred"))
            }
            Err((code, message)) => error(env, code, &message)
                .and_then(|it| checked(sys::napi_reject_deferred(env, deferred, it), "napi_reject_deferred")),
        }
    };
    if let Err(what) = settled {
        throw(env, &what);
    }
}

/// Throws a JS `Error` of `message` on this thread.
fn throw(env: sys::Env, message: &str) {
    let message = std::ffi::CString::new(message.replace('\0', "\u{fffd}")).unwrap_or_default();
    // SAFETY: `env` is the JS thread's environment in every caller, and the message is a
    // NUL-terminated string alive across the call. A throw that fails has nobody to tell.
    let _ = unsafe { sys::napi_throw_error(env, null(), message.as_ptr()) };
}

/// Holds the event loop open while a promise of the client is unsettled, and lets it go when
/// none is.
///
/// # Safety
///
/// `env` is the environment of the JS thread this runs on, and `js.tsfn` is not released.
unsafe fn hold(env: sys::Env, js: &mut Js) {
    let wanted = !js.pending.is_empty() || !js.closing.is_empty() || js.connecting.is_some();
    if wanted == js.held || js.closed {
        return;
    }
    js.held = wanted;
    // SAFETY: `env` is this thread's and the function is live, by this function's contract.
    // A failure leaves the loop held or not as it was, which settles nothing wrongly.
    let _ = unsafe {
        match wanted {
            true => sys::napi_ref_threadsafe_function(env, js.tsfn),
            false => sys::napi_unref_threadsafe_function(env, js.tsfn),
        }
    };
}

/// The threadsafe function's call, on the JS thread: takes one post of the worker.
unsafe extern "C" fn told(env: sys::Env, _callback: sys::Value, context: *mut c_void, data: *mut c_void) {
    // SAFETY: `data` is the box `Poster::post` made, handed to this call alone.
    let post = unsafe { Box::from_raw(data.cast::<Post>()) };
    if env.is_null() {
        // The environment is tearing down: nothing can be settled.
        return;
    }
    // SAFETY: `context` is the `Rc<Shared>` the function holds a strong count of until its
    // finalizer, and this is the JS thread, the only one that touches it.
    let shared = unsafe { &*context.cast::<Shared>() };
    let taken = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` is this thread's; `shared` is the client's state.
        unsafe { take(env, shared, context, *post) }
    }));
    if let Err(panic) = taken {
        throw(env, &format!("blaze-napi: a panic taking a post: {}", said(panic)));
    }
}

/// Takes one post.
///
/// # Safety
///
/// `env` is the environment of the JS thread this runs on; `context` is the raw `Rc<Shared>`
/// `shared` is.
unsafe fn take(env: sys::Env, shared: &Shared, context: *mut c_void, post: Post) {
    // SAFETY, of every block below: `env` is this thread's; each deferred is unsettled (it is
    // taken out of the state as it is settled); each reference was made by `call` and is
    // deleted once; no borrow of `shared` is held across a call into JS.
    match post {
        Post::Connected(Ok(waker)) => {
            let mut js = shared.borrow_mut();
            js.waker = Some(waker);
            let Some(deferred) = js.connecting.take() else { return };
            let mut handle = null_mut();
            // The handle's strong count, given back by `dropped`.
            let made = unsafe {
                Rc::increment_strong_count(context.cast::<Shared>());
                sys::napi_create_external(env, context, Some(dropped), null_mut(), &raw mut handle)
            };
            let settled = checked(made, "napi_create_external")
                .and_then(|()| checked(unsafe { sys::napi_resolve_deferred(env, deferred, handle) }, "napi_resolve_deferred"));
            unsafe { hold(env, &mut js) };
            drop(js);
            if let Err(what) = settled {
                throw(env, &what);
            }
        }
        Post::Connected(Err(error)) => {
            let deferred = shared.borrow_mut().connecting.take();
            if let Some(deferred) = deferred {
                unsafe { settle(env, deferred, Err((error.code(), error.to_string()))) };
            }
        }
        Post::Event { call, json } => {
            let on_event = shared.borrow().pending.iter().find(|it| it.call == call).map(|it| it.on_event);
            let Some(on_event) = on_event.filter(|it| !it.is_null()) else { return };
            let called = unsafe {
                let (mut function, mut undefined) = (null_mut(), null_mut());
                checked(sys::napi_get_reference_value(env, on_event, &raw mut function), "napi_get_reference_value")
                    .and_then(|()| checked(sys::napi_get_undefined(env, &raw mut undefined), "napi_get_undefined"))
                    .and_then(|()| string(env, &json))
                    // A callback that throws leaves its exception pending, which the host
                    // reports as uncaught: the status says so and is not this addon's error.
                    .map(|line| sys::napi_call_function(env, undefined, function, 1, &raw const line, null_mut()))
            };
            if let Err(what) = called {
                throw(env, &what);
            }
        }
        Post::Ended { call, result } => {
            let mut js = shared.borrow_mut();
            let Some(at) = js.pending.iter().position(|it| it.call == call) else { return };
            let Pending { deferred, on_event, .. } = js.pending.swap_remove(at);
            unsafe { hold(env, &mut js) };
            drop(js);
            unsafe {
                if !on_event.is_null() {
                    let _ = sys::napi_delete_reference(env, on_event);
                }
                match &result {
                    Ok(answer) => settle(env, deferred, Ok(Some(answer.as_str()))),
                    Err(error) => settle(env, deferred, Err((error.code(), error.to_string()))),
                }
            }
        }
        Post::Exited { why } => {
            let mut js = shared.borrow_mut();
            js.closed = true;
            js.waker = None;
            let pending = std::mem::take(&mut js.pending);
            let closing = std::mem::take(&mut js.closing);
            let connecting = js.connecting.take();
            let tsfn = js.tsfn;
            drop(js);
            let gone = match &why {
                Some(why) => (13, format!("blaze-napi: the client's worker panicked: {why}")),
                None => (Fault::Closed.code(), Fault::Closed.to_string()),
            };
            unsafe {
                for Pending { deferred, on_event, .. } in pending {
                    if !on_event.is_null() {
                        let _ = sys::napi_delete_reference(env, on_event);
                    }
                    settle(env, deferred, Err(gone.clone()));
                }
                if let Some(deferred) = connecting {
                    settle(env, deferred, Err(gone.clone()));
                }
                for deferred in closing {
                    settle(env, deferred, Ok(None));
                }
                // The worker's last post is taken: the function is let go, and its finalizer
                // (`finished`) gives back its strong count.
                let _ = sys::napi_release_threadsafe_function(tsfn, sys::RELEASE);
            }
        }
    }
}

/// The threadsafe function's finalizer, on the JS thread: gives back its strong count.
unsafe extern "C" fn finished(_env: sys::Env, data: *mut c_void, _hint: *mut c_void) {
    // SAFETY: `data` is the `Rc::into_raw` `connect` gave the function, given back once, here.
    drop(unsafe { Rc::from_raw(data.cast::<Shared>()) });
}

/// The handle's finalizer, on the JS thread: the client was let go without `close()`, so its
/// worker is told to close; gives back the handle's strong count.
unsafe extern "C" fn dropped(_env: sys::Env, data: *mut c_void, _hint: *mut c_void) {
    // SAFETY: `data` is the strong count `take` added for the handle, given back once, here.
    let shared = unsafe { Rc::from_raw(data.cast::<Shared>()) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let js = shared.borrow();
        if !js.closed {
            // A worker that is gone took its receiver with it: nothing to tell.
            let _ = js.commands.send(Command::Close);
            if let Some(waker) = &js.waker {
                let _ = waker.wake();
            }
        }
    }));
}

/// The arguments of a call of an exported function, at most four.
///
/// # Safety
///
/// `env` and `info` are those the host called the function with.
unsafe fn arguments(env: sys::Env, info: sys::CallbackInfo) -> Result<([sys::Value; 4], usize), String> {
    let mut argv = [null_mut(); 4];
    let mut argc = argv.len();
    // SAFETY: `argv` has room for `argc` values, which the call fills and counts.
    let status = unsafe { sys::napi_get_cb_info(env, info, &raw mut argc, argv.as_mut_ptr(), null_mut(), null_mut()) };
    checked(status, "napi_get_cb_info").map(|()| (argv, argc))
}

/// A new promise: its deferred and the promise.
///
/// # Safety
///
/// `env` is the environment of the JS thread this runs on.
unsafe fn promise(env: sys::Env) -> Result<(sys::Deferred, sys::Value), String> {
    let (mut deferred, mut value) = (null_mut(), null_mut());
    // SAFETY: both out pointers are locals the call writes.
    let status = unsafe { sys::napi_create_promise(env, &raw mut deferred, &raw mut value) };
    checked(status, "napi_create_promise").map(|()| (deferred, value))
}

/// The client a handle is.
///
/// # Safety
///
/// `env` is the environment of the JS thread this runs on, and `handle` a value of it. A
/// handle is only ever made by `take`, of an `Rc<Shared>` it keeps alive.
unsafe fn client<'a>(env: sys::Env, handle: sys::Value) -> Result<&'a Shared, String> {
    let mut data = null_mut();
    // SAFETY: the out pointer is a local the call writes.
    let status = unsafe { sys::napi_get_value_external(env, handle, &raw mut data) };
    if status != sys::OK || data.is_null() {
        return Err("blaze-napi: the first argument must be a client's handle, from connect()".to_owned());
    }
    // SAFETY: the external's data is the `Rc<Shared>` whose strong count the handle holds.
    Ok(unsafe { &*data.cast::<Shared>() })
}

/// Runs an exported function's body: an error or a panic in it is thrown to JS.
fn exported(env: sys::Env, body: impl FnOnce() -> Result<sys::Value, String>) -> sys::Value {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => value,
        Ok(Err(what)) => {
            throw(env, &what);
            null_mut()
        }
        Err(panic) => {
            throw(env, &format!("blaze-napi: a panic: {}", said(panic)));
            null_mut()
        }
    }
}

/// `connect(optionsJson: string): Promise<handle>`.
unsafe extern "C" fn connect(env: sys::Env, info: sys::CallbackInfo) -> sys::Value {
    exported(env, || {
        // SAFETY, of the blocks below: `env` and `info` are the host's for this call, on the
        // JS thread; out pointers are locals.
        let (argv, argc) = unsafe { arguments(env, info)? };
        if argc < 1 {
            return Err("blaze-napi: connect(optionsJson) takes its options".to_owned());
        }
        let options = unsafe { text(env, argv[0], "connect's options")? };
        let (deferred, value) = unsafe { promise(env)? };
        let (commands, received) = crossbeam_channel::unbounded();
        // alloc: one client's state, shared by its handle and its threadsafe function.
        let shared = Rc::new(RefCell::new(Js {
            tsfn: null_mut(),
            commands,
            waker: None,
            connecting: Some(deferred),
            next_call: 1,
            pending: Vec::new(),
            closing: Vec::new(),
            closed: false,
            held: true,
        }));
        // The function's strong count, given back by `finished`.
        let context = Rc::into_raw(Rc::clone(&shared)).cast_mut().cast::<c_void>();
        let mut tsfn = null_mut();
        let made = unsafe {
            let name = string(env, "blaze client")?;
            sys::napi_create_threadsafe_function(
                env,
                null_mut(),
                null_mut(),
                name,
                0,
                1,
                context,
                Some(finished),
                context,
                Some(told),
                &raw mut tsfn,
            )
        };
        if let Err(what) = checked(made, "napi_create_threadsafe_function") {
            // SAFETY: the function was not made, so its finalizer never runs: the count
            // given to it is taken back here.
            drop(unsafe { Rc::from_raw(context.cast::<Shared>()) });
            return Err(what);
        }
        shared.borrow_mut().tsfn = tsfn;
        let poster = Poster(tsfn);
        let spawned = std::thread::Builder::new()
            .name("blaze-client".to_owned())
            .spawn(move || work(poster, received, options));
        if let Err(why) = spawned {
            // SAFETY: the function is live and unreleased; releasing it runs `finished`.
            let _ = unsafe { sys::napi_release_threadsafe_function(tsfn, sys::RELEASE) };
            shared.borrow_mut().closed = true;
            return Err(format!("blaze-napi: starting the client's thread: {why}"));
        }
        Ok(value)
    })
}

/// `call(handle, method: string, optionsJson: string, onEvent?: (line: string) => void):
/// Promise<string>`.
unsafe extern "C" fn call(env: sys::Env, info: sys::CallbackInfo) -> sys::Value {
    exported(env, || {
        // SAFETY, of the blocks below: as `connect`'s.
        let (argv, argc) = unsafe { arguments(env, info)? };
        if argc < 3 {
            return Err("blaze-napi: call(handle, method, optionsJson, onEvent?) takes three arguments or four".to_owned());
        }
        let shared = unsafe { client(env, argv[0])? };
        let method = unsafe { text(env, argv[1], "call's method")? };
        let options = unsafe { text(env, argv[2], "call's options")? };
        let mut on_event = null_mut();
        if argc > 3 {
            let mut kind = 0;
            checked(unsafe { sys::napi_typeof(env, argv[3], &raw mut kind) }, "napi_typeof")?;
            if kind == sys::FUNCTION {
                let made = unsafe { sys::napi_create_reference(env, argv[3], 1, &raw mut on_event) };
                checked(made, "napi_create_reference")?;
            }
        }
        let (deferred, value) = unsafe { promise(env)? };
        let mut js = shared.borrow_mut();
        if js.closed {
            drop(js);
            unsafe {
                if !on_event.is_null() {
                    let _ = sys::napi_delete_reference(env, on_event);
                }
                settle(env, deferred, Err((Fault::Closed.code(), "the client is closed".to_owned())));
            }
            return Ok(value);
        }
        let call = js.next_call;
        js.next_call += 1;
        js.pending.push(Pending { call, deferred, on_event });
        unsafe { hold(env, &mut js) };
        // The worker is alive (not closed), so it holds the receiver and its waker is here.
        let sent = js.commands.send(Command::Call { call, method, options });
        let woken = js.waker.as_ref().map(Waker::wake);
        drop(js);
        if sent.is_err() || !matches!(woken, Some(Ok(()))) {
            return Err("blaze-napi: the client's worker is gone before its last post: a bug in blaze-napi".to_owned());
        }
        Ok(value)
    })
}

/// `close(handle): Promise<void>`: settled once the worker is gone and every call under way
/// has been rejected as closed.
unsafe extern "C" fn close(env: sys::Env, info: sys::CallbackInfo) -> sys::Value {
    exported(env, || {
        // SAFETY, of the blocks below: as `connect`'s.
        let (argv, argc) = unsafe { arguments(env, info)? };
        if argc < 1 {
            return Err("blaze-napi: close(handle) takes the client's handle".to_owned());
        }
        let shared = unsafe { client(env, argv[0])? };
        let (deferred, value) = unsafe { promise(env)? };
        let mut js = shared.borrow_mut();
        if js.closed {
            drop(js);
            unsafe { settle(env, deferred, Ok(None)) };
            return Ok(value);
        }
        let first = js.closing.is_empty();
        js.closing.push(deferred);
        unsafe { hold(env, &mut js) };
        if first {
            let _ = js.commands.send(Command::Close);
            if let Some(waker) = &js.waker {
                let _ = waker.wake();
            }
        }
        Ok(value)
    })
}

/// The addon's registration, which the host calls when it loads it: `exports.connect`,
/// `exports.call` and `exports.close`.
///
/// # Safety
///
/// Called by a Node-API host, on its JS thread, with its environment and the module's exports.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_register_module_v1(env: sys::Env, exports: sys::Value) -> sys::Value {
    exported(env, || {
        let functions: [(&std::ffi::CStr, sys::Callback); 3] =
            [(c"connect", connect), (c"call", call), (c"close", close)];
        for (name, function) in functions {
            let mut made = null_mut();
            // SAFETY: `env` and `exports` are the host's; the name is NUL-terminated and its
            // length given; `made` is written by the first call and read by the second.
            unsafe {
                let name_ptr: *const c_char = name.as_ptr();
                checked(
                    sys::napi_create_function(env, name_ptr, name.count_bytes(), function, null_mut(), &raw mut made),
                    "napi_create_function",
                )?;
                checked(sys::napi_set_named_property(env, exports, name_ptr, made), "napi_set_named_property")?;
            }
        }
        Ok(exports)
    })
}
