//! The addon on the JS thread: Node-API as declared by hand ([`sys`]), a client's state
//! ([`Js`]), the three exported functions, and what the threadsafe function and the
//! finalizers do. All of it runs on the JS thread but [`Poster::post`], the worker's one call.
//!
//! What Node-API promises, which the `SAFETY` comments below lean on by these names:
//!
//! - *The call's `env`*: the `napi_env` the host hands a native callback, a threadsafe
//!   function's `call_js` or a finalizer is valid until that call returns, on the thread it
//!   was called on, the environment's JS thread.
//! - *A value of the call*: a `napi_value` the host hands such a call, or one a Node-API
//!   function gives during it, is valid until the call returns: the host opens a handle scope
//!   around each.
//! - *A deferred* is valid from `napi_create_promise` until it is resolved or rejected, once.
//! - *A reference* is valid from `napi_create_reference` until `napi_delete_reference`.
//! - *A threadsafe function* is valid, and callable from any thread, from its creation until
//!   the release of its last thread (a client's has one, `initial_thread_count` 1) or, for a
//!   thread, until a call of it answers that it is closing.
//! - An out pointer is written during the function it is passed to and not kept.
//!
//! What this module keeps of a client's state ([`Js`]), on the JS thread alone:
//!
//! - Every deferred in `connecting`, `pending` and `closing` is unsettled: one is taken out
//!   of the state before it is settled, so it is settled once.
//! - Every `on_event` in `pending` that is not null is a reference not yet deleted: it is
//!   deleted as its entry is taken out.
//! - `tsfn` is not released while `closed` is false.
//! - The state is alive while its handle or its threadsafe function is: each holds a strong
//!   count of the `Rc`, given back by its finalizer ([`dropped`], [`finished`]).

use std::{
    cell::{Cell, RefCell},
    ffi::{c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{null, null_mut},
    rc::Rc,
};

use blaze_client::{Fault, Waker};
use crossbeam_channel::Sender;

use crate::worker::{Command, Post, said, work};

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

thread_local! {
    /// The clients of this JS thread that JS holds a handle of: for each, the pointer its
    /// handle's external carries, which is its `Rc<Shared>`'s. One is added as its handle is
    /// made ([`take`]) and removed by the handle's finalizer ([`dropped`]), which gives back
    /// the strong count the handle holds: so a pointer found here is of a live `Shared`.
    /// [`client`] takes no external's pointer for a client's unless it is here, whatever JS
    /// passes it (an external of another addon, say).
    static HANDLES: RefCell<Vec<*const Shared>> = const { RefCell::new(Vec::new()) };
}

/// The threadsafe function, for the worker: Node-API lets any thread call one.
pub(crate) struct Poster {
    tsfn: sys::Tsfn,
    /// A post was answered that the function is closing (the environment is going): Node-API
    /// forbids a thread any further call of it.
    closing: Cell<bool>,
}

// SAFETY: a threadsafe function is made to be called from other threads than the one that
// made it; the worker calls only `napi_call_threadsafe_function` on it, and stops before the
// JS thread releases it (the release follows the worker's last post, `Post::Exited`). The
// `Cell` is the worker's alone: a `Poster` is moved to the worker and is not `Sync`.
unsafe impl Send for Poster {}

impl Poster {
    /// Hands `post` to the JS thread; dropped when the environment is going.
    pub(crate) fn post(&self, post: Post) {
        if self.closing.get() {
            return;
        }
        // alloc: one box a post, freed by the JS thread's `told`.
        let data = Box::into_raw(Box::new(post));
        // SAFETY: the function is valid for this thread: the JS thread releases it only as it
        // takes `Post::Exited`, the worker's last post (`work`), or with no worker started
        // (`connect`), and no call of it by this thread was answered that it is closing
        // (`self.closing`). `data` is a box the function's `call_js` (`told`) takes.
        let status =
            unsafe { sys::napi_call_threadsafe_function(self.tsfn, data.cast(), sys::NONBLOCKING) };
        if status != sys::OK {
            // The environment is tearing down: nobody takes the post, now or later.
            self.closing.set(true);
            // SAFETY: a call that fails queued nothing, so `data` is still the box made
            // above, held by nothing else.
            drop(unsafe { Box::from_raw(data) });
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
/// `env` is the call's `env`, on the JS thread.
unsafe fn string(env: sys::Env, text: &str) -> Result<sys::Value, String> {
    let mut value = null_mut();
    // SAFETY: the call's `env`, by this function's contract; `text` is readable for the
    // length given with it, so no NUL is looked for; `value` is a local the call writes.
    let status =
        unsafe { sys::napi_create_string_utf8(env, text.as_ptr().cast(), text.len(), &raw mut value) };
    checked(status, "napi_create_string_utf8").map(|()| value)
}

/// The text of the JS string `value`.
///
/// # Safety
///
/// `env` is the call's `env`, on the JS thread, and `value` a value of the call.
unsafe fn text(env: sys::Env, value: sys::Value, what: &str) -> Result<String, String> {
    let mut len = 0;
    // SAFETY: `env` and `value` are the call's, by this function's contract; a null buffer
    // asks for the length alone, which the call writes to the local `len`.
    let status = unsafe { sys::napi_get_value_string_utf8(env, value, null_mut(), 0, &raw mut len) };
    checked(status, "napi_get_value_string_utf8")
        .map_err(|_| format!("blaze-napi: {what} must be a string"))?;
    let mut bytes = vec![0u8; len + 1]; // alloc: the argument's text, which the call owns
    let mut written = 0;
    // SAFETY: `env` and `value` as above; the buffer is `bytes.len()` writable bytes, the
    // size given, of which the call writes at most that many, the NUL counted, and says in
    // the local `written` how many without it.
    let status = unsafe {
        sys::napi_get_value_string_utf8(env, value, bytes.as_mut_ptr().cast(), bytes.len(), &raw mut written)
    };
    checked(status, "napi_get_value_string_utf8")?;
    bytes.truncate(written);
    String::from_utf8(bytes).map_err(|_| format!("blaze-napi: {what} is not UTF-8"))
}

/// A JS `Error` of `message` whose `code` is `code` ([`blaze_client::Error::code`]; 13 for a
/// panic).
///
/// # Safety
///
/// `env` is the call's `env`, on the JS thread.
unsafe fn error(env: sys::Env, code: u32, message: &str) -> Result<sys::Value, String> {
    let (mut made, mut number) = (null_mut(), null_mut());
    // SAFETY: the call's `env`, by this function's contract, which is `string`'s too; each
    // value passed on was made by the call before it, in this same call of the host; each out
    // pointer is a local its call writes; the property's name is a NUL-terminated literal.
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
/// `env` is the call's `env`, on the JS thread, and `deferred` a deferred of it, not settled
/// before and not used after.
unsafe fn settle(env: sys::Env, deferred: sys::Deferred, result: Result<Option<&str>, (u32, String)>) {
    // SAFETY: the call's `env` and an unsettled `deferred`, by this function's contract,
    // settled by exactly one arm; the value it is settled with was made just before in the
    // same arm, so it is a value of the call; `undefined` is a local its call writes.
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
        // SAFETY: the call's `env`, by this function's contract.
        unsafe { throw(env, &what) };
    }
}

/// Throws a JS `Error` of `message`.
///
/// # Safety
///
/// `env` is the call's `env`, on the JS thread.
unsafe fn throw(env: sys::Env, message: &str) {
    let message = std::ffi::CString::new(message.replace('\0', "\u{fffd}")).unwrap_or_default();
    // SAFETY: the call's `env`, by this function's contract; a null code is allowed, and the
    // message is a NUL-terminated string alive across the call, which copies it. A throw that
    // fails has nobody to tell.
    let _ = unsafe { sys::napi_throw_error(env, null(), message.as_ptr()) };
}

/// Holds the event loop open while a promise of the client is unsettled, and lets it go when
/// none is. Nothing is done for a client that is closed: its function may be released.
///
/// # Safety
///
/// `env` is the call's `env`, on the JS thread, and `js` a client's state as this module
/// keeps it (`tsfn` not released while `closed` is false).
unsafe fn hold(env: sys::Env, js: &mut Js) {
    let wanted = !js.pending.is_empty() || !js.closing.is_empty() || js.connecting.is_some();
    if wanted == js.held || js.closed {
        return;
    }
    js.held = wanted;
    // SAFETY: the call's `env`, by this function's contract; `js.closed` is false here, so
    // `js.tsfn` is not released and is valid. A failure leaves the loop held or not as it was,
    // which settles nothing wrongly.
    let _ = unsafe {
        match wanted {
            true => sys::napi_ref_threadsafe_function(env, js.tsfn),
            false => sys::napi_unref_threadsafe_function(env, js.tsfn),
        }
    };
}

/// The threadsafe function's call, on the JS thread: takes one post of the worker.
unsafe extern "C" fn told(env: sys::Env, _callback: sys::Value, context: *mut c_void, data: *mut c_void) {
    // SAFETY: `data` is what the host queued for this call, which is the box `Poster::post`
    // made of a `Post` and gave up: this call alone has it.
    let post = unsafe { Box::from_raw(data.cast::<Post>()) };
    if env.is_null() {
        // The environment is tearing down: nothing can be settled, and the post is freed.
        return;
    }
    // SAFETY: `context` is the `Rc::into_raw` of an `Rc<Shared>` that `connect` gave the
    // function with a strong count, which only the function's finalizer (`finished`) gives
    // back, and the host calls that after the function's last `call_js`. This is the JS
    // thread, the only one that touches a `Shared`, and the reference is not used after `take`
    // (which may release the function) returns.
    let shared = unsafe { &*context.cast::<Shared>() };
    let taken = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` is this call's, not null, on the JS thread; `context` is the raw
        // `Rc<Shared>` that `shared` was read through, just above.
        unsafe { take(env, shared, context, *post) }
    }));
    if let Err(panic) = taken {
        // SAFETY: `env` is this call's, not null.
        unsafe { throw(env, &format!("blaze-napi: a panic taking a post: {}", said(&*panic))) };
    }
}

/// Takes one post. No borrow of `shared` is held across a call into JS (`onEvent`), which
/// may call the exported functions on the same client.
///
/// # Safety
///
/// `env` is the call's `env`, on the JS thread; `context` is the `Rc::into_raw` of the live
/// `Rc<Shared>` that `shared` borrows, a client's state as this module keeps it.
unsafe fn take(env: sys::Env, shared: &Shared, context: *mut c_void, post: Post) {
    match post {
        Post::Connected(Ok(waker)) => {
            let mut js = shared.borrow_mut();
            js.waker = Some(waker);
            let Some(deferred) = js.connecting.take() else { return };
            // SAFETY: `context` is the raw pointer of a live `Rc<Shared>`, by this function's
            // contract. The count added is the handle's: `dropped` gives it back, or the
            // failure arm below when no handle is made.
            unsafe { Rc::increment_strong_count(context.cast::<Shared>()) };
            let mut handle = null_mut();
            // SAFETY: the call's `env`; `handle` is a local the call writes; the external
            // carries `context` with the count added above, which its finalizer `dropped`
            // takes as such.
            let made =
                unsafe { sys::napi_create_external(env, context, Some(dropped), null_mut(), &raw mut handle) };
            let settled = match checked(made, "napi_create_external") {
                Ok(()) => {
                    // alloc: a pointer a client, kept until its handle is collected
                    HANDLES.with(|it| it.borrow_mut().push(context.cast::<Shared>().cast_const()));
                    // SAFETY: the call's `env`; `deferred` was taken out of `connecting` just
                    // above, so it is unsettled and this is its one settling; `handle` is a
                    // value of the call, made above.
                    let status = unsafe { sys::napi_resolve_deferred(env, deferred, handle) };
                    checked(status, "napi_resolve_deferred")
                }
                Err(what) => {
                    // SAFETY: no external was made, so `dropped` never runs for it: the count
                    // added for the handle is given back here. The state stays alive on the
                    // threadsafe function's count.
                    unsafe { Rc::decrement_strong_count(context.cast::<Shared>()) };
                    Err(what)
                }
            };
            // SAFETY: the call's `env`; `js` is the client's state, by this function's contract.
            unsafe { hold(env, &mut js) };
            drop(js);
            if let Err(what) = settled {
                // SAFETY: the call's `env`.
                unsafe { throw(env, &what) };
            }
        }
        Post::Connected(Err(error)) => {
            let deferred = shared.borrow_mut().connecting.take();
            if let Some(deferred) = deferred {
                // SAFETY: the call's `env`; `deferred` was taken out of `connecting`, so it
                // is unsettled, and nothing has it after this.
                unsafe { settle(env, deferred, Err((error.code(), error.to_string()))) };
            }
        }
        Post::Event { call, json } => {
            let on_event = shared.borrow().pending.iter().find(|it| it.call == call).map(|it| it.on_event);
            let Some(on_event) = on_event.filter(|it| !it.is_null()) else { return };
            // SAFETY: the call's `env`; `on_event` was read from an entry of `pending` just
            // above, with no call into JS since, so it is a reference not yet deleted;
            // `function`, `undefined` and `line` are values of the call, each written by the
            // call before its use; one argument is passed and one is counted; a null result
            // pointer asks for no result.
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
                // SAFETY: the call's `env`.
                unsafe { throw(env, &what) };
            }
        }
        Post::Ended { call, result } => {
            let mut js = shared.borrow_mut();
            let Some(at) = js.pending.iter().position(|it| it.call == call) else { return };
            let Pending { deferred, on_event, .. } = js.pending.swap_remove(at);
            // SAFETY: the call's `env`; `js` is the client's state.
            unsafe { hold(env, &mut js) };
            drop(js);
            if !on_event.is_null() {
                // SAFETY: the call's `env`; `on_event` came out of `pending` with its entry
                // just above, so it is a reference not yet deleted, and this is its one
                // deletion. One that fails leaks the reference, no more.
                let _ = unsafe { sys::napi_delete_reference(env, on_event) };
            }
            let result = match &result {
                Ok(answer) => Ok(Some(answer.as_str())),
                Err(error) => Err((error.code(), error.to_string())),
            };
            // SAFETY: the call's `env`; `deferred` came out of `pending` with its entry, so
            // it is unsettled, and nothing has it after this.
            unsafe { settle(env, deferred, result) };
        }
        Post::Exited { why } => {
            let mut js = shared.borrow_mut();
            let released = js.closed;
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
            for Pending { deferred, on_event, .. } in pending {
                if !on_event.is_null() {
                    // SAFETY: the call's `env`; `on_event` came out of `pending`, taken whole
                    // above, so it is a reference not yet deleted, deleted once, here.
                    let _ = unsafe { sys::napi_delete_reference(env, on_event) };
                }
                // SAFETY: the call's `env`; `deferred` came out of `pending`, taken whole
                // above, so it is unsettled, and nothing has it after this.
                unsafe { settle(env, deferred, Err(gone.clone())) };
            }
            if let Some(deferred) = connecting {
                // SAFETY: the call's `env`; `deferred` was taken out of `connecting` above.
                unsafe { settle(env, deferred, Err(gone.clone())) };
            }
            for deferred in closing {
                // SAFETY: the call's `env`; `deferred` came out of `closing`, taken whole
                // above, so it is unsettled, and nothing has it after this.
                unsafe { settle(env, deferred, Ok(None)) };
            }
            if !released {
                // The worker's last post is taken: the function is let go, and its finalizer
                // (`finished`) gives back its strong count.
                // SAFETY: `closed` was false until this post, so `tsfn` was not released
                // (`released`), and this is the one release of its one thread; the worker
                // posts no more (`Post::Exited` is its last, `work`). Nothing here reads the
                // state after it, should the host run the finalizer at once.
                let _ = unsafe { sys::napi_release_threadsafe_function(tsfn, sys::RELEASE) };
            }
        }
    }
}

/// The threadsafe function's finalizer, on the JS thread: gives back its strong count.
unsafe extern "C" fn finished(_env: sys::Env, data: *mut c_void, _hint: *mut c_void) {
    // SAFETY: `data` is the function's finalize data, the `Rc::into_raw` whose count `connect`
    // gave the function; the host finalizes a function once, so the count is given back
    // once, here, on the JS thread, where every other count of the `Rc` lives.
    drop(unsafe { Rc::from_raw(data.cast::<Shared>()) });
}

/// The handle's finalizer, on the JS thread: the client was let go without `close()`, so its
/// worker is told to close; gives back the handle's strong count.
unsafe extern "C" fn dropped(_env: sys::Env, data: *mut c_void, _hint: *mut c_void) {
    let data = data.cast::<Shared>().cast_const();
    // The handle is gone: no external carries the pointer any more. A thread whose locals
    // are gone runs no JS that could pass one.
    let _ = HANDLES.try_with(|it| it.borrow_mut().retain(|held| !std::ptr::eq(*held, data)));
    // SAFETY: `data` is the external's data, the pointer of the `Rc<Shared>` with the strong
    // count `take` added for this handle; the host finalizes an external once, so the count
    // is given back once, here, on the JS thread.
    let shared = unsafe { Rc::from_raw(data) };
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
/// `env` and `info` are those the host called the exported function with, during that call.
unsafe fn arguments(env: sys::Env, info: sys::CallbackInfo) -> Result<([sys::Value; 4], usize), String> {
    let mut argv = [null_mut(); 4];
    let mut argc = argv.len();
    // SAFETY: `env` and `info` are the call's, by this function's contract; `argv` has room
    // for the `argc` values asked for, and the call writes at most that many and then, to
    // `argc`, how many the caller passed; null asks for neither `this` nor the data.
    let status =
        unsafe { sys::napi_get_cb_info(env, info, &raw mut argc, argv.as_mut_ptr(), null_mut(), null_mut()) };
    checked(status, "napi_get_cb_info").map(|()| (argv, argc))
}

/// A new promise: its deferred and the promise.
///
/// # Safety
///
/// `env` is the call's `env`, on the JS thread.
unsafe fn promise(env: sys::Env) -> Result<(sys::Deferred, sys::Value), String> {
    let (mut deferred, mut value) = (null_mut(), null_mut());
    // SAFETY: the call's `env`, by this function's contract; both out pointers are locals
    // the call writes.
    let status = unsafe { sys::napi_create_promise(env, &raw mut deferred, &raw mut value) };
    checked(status, "napi_create_promise").map(|()| (deferred, value))
}

/// The client a handle is, for the length of the exported function's call.
///
/// # Safety
///
/// `env` is the call's `env`, on the JS thread, and `handle` a value of the call. The
/// reference is not kept past the call: a handle that is an argument of the call is not
/// collected during it, so its strong count is held throughout.
unsafe fn client<'a>(env: sys::Env, handle: sys::Value) -> Result<&'a Shared, String> {
    let mut data = null_mut();
    // SAFETY: `env` and `handle` are the call's, by this function's contract; the out pointer
    // is a local the call writes. A value that is no external is an error status.
    let status = unsafe { sys::napi_get_value_external(env, handle, &raw mut data) };
    let data = data.cast::<Shared>().cast_const();
    let ours = || HANDLES.with(|it| it.borrow().iter().any(|held| std::ptr::eq(*held, data)));
    if status != sys::OK || !ours() {
        return Err("blaze-napi: the first argument must be a client's handle, from connect()".to_owned());
    }
    // SAFETY: `data` is in `HANDLES`, so it is the pointer of an `Rc<Shared>` whose handle
    // was made and not finalized: the handle's strong count keeps the `Shared` alive, for as
    // long as the caller may use the reference by this function's contract. This is the JS
    // thread, the only one with a `Shared`.
    Ok(unsafe { &*data })
}

/// Runs an exported function's body: an error or a panic in it is thrown to JS.
///
/// # Safety
///
/// `env` is the call's `env`, on the JS thread.
unsafe fn exported(env: sys::Env, body: impl FnOnce() -> Result<sys::Value, String>) -> sys::Value {
    let what = match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => return value,
        Ok(Err(what)) => what,
        Err(panic) => format!("blaze-napi: a panic: {}", said(&*panic)),
    };
    // SAFETY: the call's `env`, by this function's contract.
    unsafe { throw(env, &what) };
    null_mut()
}

/// `connect(optionsJson: string): Promise<handle>`.
unsafe extern "C" fn connect(env: sys::Env, info: sys::CallbackInfo) -> sys::Value {
    let body = || -> Result<sys::Value, String> {
        // SAFETY: `env` and `info` are what the host called this function with, and this is
        // that call.
        let (argv, argc) = unsafe { arguments(env, info)? };
        if argc < 1 {
            return Err("blaze-napi: connect(optionsJson) takes its options".to_owned());
        }
        // SAFETY: the call's `env`; `argv[0]` is an argument of the call (`argc` is at least
        // 1), so a value of it.
        let options = unsafe { text(env, argv[0], "connect's options")? };
        // SAFETY: the call's `env`.
        let (deferred, value) = unsafe { promise(env)? };
        // SAFETY: the call's `env`.
        let name = unsafe { string(env, "blaze client")? };
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
        // SAFETY: the call's `env`; `name` is a value of the call, made above; no JS function
        // is given, which is allowed with a `call_js` (`told`); `context` is the function's
        // finalize data and its context both, with the strong count taken above, which
        // `finished` gives back after the last `told`; `tsfn` is a local the call writes.
        let made = unsafe {
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
            // given to it, `context`'s, is taken back here, once.
            drop(unsafe { Rc::from_raw(context.cast::<Shared>()) });
            return Err(what);
        }
        shared.borrow_mut().tsfn = tsfn;
        let poster = Poster { tsfn, closing: Cell::new(false) };
        let spawned = std::thread::Builder::new()
            .name("blaze-client".to_owned())
            .spawn(move || work(&poster, &received, &options));
        if let Err(why) = spawned {
            shared.borrow_mut().closed = true;
            // SAFETY: the function was made above and not released, and no worker runs to
            // call it: this is the one release of its one thread, after which `finished`
            // gives back its count. `closed` is set, so nothing uses `tsfn` after.
            let _ = unsafe { sys::napi_release_threadsafe_function(tsfn, sys::RELEASE) };
            return Err(format!("blaze-napi: starting the client's thread: {why}"));
        }
        Ok(value)
    };
    // SAFETY: `env` is what the host called this function with, on the JS thread.
    unsafe { exported(env, body) }
}

/// `call(handle, method: string, optionsJson: string, onEvent?: (line: string) => void):
/// Promise<string>`.
unsafe extern "C" fn call(env: sys::Env, info: sys::CallbackInfo) -> sys::Value {
    let body = || -> Result<sys::Value, String> {
        // SAFETY: `env` and `info` are what the host called this function with, and this is
        // that call.
        let (argv, argc) = unsafe { arguments(env, info)? };
        if argc < 3 {
            return Err("blaze-napi: call(handle, method, optionsJson, onEvent?) takes three arguments or four".to_owned());
        }
        // SAFETY: the call's `env`; `argv[0]` is an argument of the call (`argc` is at least
        // 3), and the reference is used only in this body, which is the call.
        let shared = unsafe { client(env, argv[0])? };
        // SAFETY: the call's `env`; `argv[1]` is an argument of the call.
        let method = unsafe { text(env, argv[1], "call's method")? };
        // SAFETY: the call's `env`; `argv[2]` is an argument of the call.
        let options = unsafe { text(env, argv[2], "call's options")? };
        let mut kind = 0;
        if argc > 3 {
            // SAFETY: the call's `env`; `argv[3]` is an argument of the call (`argc` is more
            // than 3); `kind` is a local the call writes.
            let status = unsafe { sys::napi_typeof(env, argv[3], &raw mut kind) };
            checked(status, "napi_typeof")?;
        }
        // SAFETY: the call's `env`.
        let (deferred, value) = unsafe { promise(env)? };
        if shared.borrow().closed {
            let closed = (Fault::Closed.code(), "the client is closed".to_owned());
            // SAFETY: the call's `env`; `deferred` was made just above and is in no state,
            // so it is unsettled, and nothing has it after this.
            unsafe { settle(env, deferred, Err(closed)) };
            return Ok(value);
        }
        // The call's `onEvent`, referenced last, so nothing between here and its entry in
        // `pending` can fail and leave it.
        let mut on_event = null_mut();
        if kind == sys::FUNCTION {
            // SAFETY: the call's `env`; `argv[3]` is an argument of the call, a function
            // (`kind`, read above); `on_event` is a local the call writes. The reference is
            // owned by the call's entry in `pending`, pushed below, and deleted as that
            // entry is taken out.
            let made = unsafe { sys::napi_create_reference(env, argv[3], 1, &raw mut on_event) };
            checked(made, "napi_create_reference")?;
        }
        let mut js = shared.borrow_mut();
        let call = js.next_call;
        js.next_call += 1;
        js.pending.push(Pending { call, deferred, on_event });
        // SAFETY: the call's `env`; `js` is the client's state, as this module keeps it.
        unsafe { hold(env, &mut js) };
        // The worker is alive (not closed), so it holds the receiver and its waker is here.
        let sent = js.commands.send(Command::Call { call, method, options });
        let woken = js.waker.as_ref().map(Waker::wake);
        drop(js);
        if sent.is_err() || !matches!(woken, Some(Ok(()))) {
            return Err("blaze-napi: the client's worker is gone before its last post: a bug in blaze-napi".to_owned());
        }
        Ok(value)
    };
    // SAFETY: `env` is what the host called this function with, on the JS thread.
    unsafe { exported(env, body) }
}

/// `close(handle): Promise<void>`: settled once the worker is gone and every call under way
/// has been rejected as closed.
unsafe extern "C" fn close(env: sys::Env, info: sys::CallbackInfo) -> sys::Value {
    let body = || -> Result<sys::Value, String> {
        // SAFETY: `env` and `info` are what the host called this function with, and this is
        // that call.
        let (argv, argc) = unsafe { arguments(env, info)? };
        if argc < 1 {
            return Err("blaze-napi: close(handle) takes the client's handle".to_owned());
        }
        // SAFETY: the call's `env`; `argv[0]` is an argument of the call (`argc` is at least
        // 1), and the reference is used only in this body, which is the call.
        let shared = unsafe { client(env, argv[0])? };
        // SAFETY: the call's `env`.
        let (deferred, value) = unsafe { promise(env)? };
        let mut js = shared.borrow_mut();
        if js.closed {
            drop(js);
            // SAFETY: the call's `env`; `deferred` was made just above and is in no state,
            // so it is unsettled, and nothing has it after this.
            unsafe { settle(env, deferred, Ok(None)) };
            return Ok(value);
        }
        let first = js.closing.is_empty();
        js.closing.push(deferred);
        // SAFETY: the call's `env`; `js` is the client's state, as this module keeps it.
        unsafe { hold(env, &mut js) };
        if first {
            let _ = js.commands.send(Command::Close);
            if let Some(waker) = &js.waker {
                let _ = waker.wake();
            }
        }
        Ok(value)
    };
    // SAFETY: `env` is what the host called this function with, on the JS thread.
    unsafe { exported(env, body) }
}

/// The addon's registration, which the host calls when it loads it: `exports.connect`,
/// `exports.call` and `exports.close`.
///
/// # Safety
///
/// Called by a Node-API host, on its JS thread, with its environment and the module's exports.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_register_module_v1(env: sys::Env, exports: sys::Value) -> sys::Value {
    let body = || -> Result<sys::Value, String> {
        let functions: [(&std::ffi::CStr, sys::Callback); 3] =
            [(c"connect", connect), (c"call", call), (c"close", close)];
        for (name, function) in functions {
            let mut made = null_mut();
            // SAFETY: `env` and `exports` are the host's for this call, by this function's
            // contract; the name is NUL-terminated and its length is given; each callback is
            // a function of this addon, which outlives the functions made of it (the host
            // keeps a loaded addon); `made` is written by the first call and read by the
            // second.
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
    };
    // SAFETY: `env` is the host's for this call, on its JS thread, by this function's contract.
    unsafe { exported(env, body) }
}
