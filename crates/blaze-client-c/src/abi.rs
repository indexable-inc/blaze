//! The functions and types of `include/blaze_client.h`.
//!
//! Every function is a thin shell over [`foreign`]. The pointer rules are the header's, said
//! again on each function. Nothing unwinds into the caller: each body runs under
//! `catch_unwind`, and a panic is the error [`PANIC`], after which the client takes no more
//! calls (its state may be half changed).
//!
//! The header's `blaze_client` is a type of this module, so here the crate blaze-client is
//! named from the root, `::blaze_client`: the bare name is the struct.

use std::{
    ffi::{CStr, CString, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    time::Duration,
};

use blaze_api::{Envelope, Request, Response, json};
use ::blaze_client::{Client, Error, ask::Answer, foreign};

/// The code of what is no request ([`Error::Asked`]'s).
const ASKED: u32 = 12;
/// The code of a panic inside the library.
const PANIC: u32 = 13;
/// The code of a null or non-UTF-8 argument.
const ARGUMENT: u32 = 14;

/// A client, as C holds it.
#[expect(non_camel_case_types, reason = "the C name")]
pub struct blaze_client {
    client: Client,
    /// A call panicked: the client's state is not known, and it takes no more.
    poisoned: bool,
}

/// An error, as C reads it: `code` 0 for none, `message` owned by the library.
#[repr(C)]
#[expect(non_camel_case_types, reason = "the C name")]
pub struct blaze_error {
    pub code: u32,
    pub message: *mut c_char,
}

/// One response's bytes, valid during the call.
#[expect(non_camel_case_types, reason = "the C name")]
pub type blaze_event_fn = Option<unsafe extern "C" fn(user: *mut c_void, bytes: *const u8, len: usize)>;

/// [`blaze_client_request`]'s formats.
const JSON: u32 = 0;
const CBOR: u32 = 1;

/// What a call failed with, before it is written out.
struct Failed {
    code: u32,
    message: String,
}

impl From<Error> for Failed {
    fn from(error: Error) -> Failed {
        Failed { code: error.code(), message: error.to_string() }
    }
}

fn argument(what: &str) -> Failed {
    Failed { code: ARGUMENT, message: what.to_owned() }
}

/// A string for C: `text` with any NUL in it written as U+FFFD, since a C string ends at one.
fn c_string(text: String) -> *mut c_char {
    let text = match text.contains('\0') {
        true => text.replace('\0', "\u{fffd}"),
        false => text,
    };
    // No NUL is left in it.
    CString::new(text).map_or(std::ptr::null_mut(), CString::into_raw)
}

/// Writes `result`'s error, or none, to `error`.
///
/// # Safety
///
/// `error` is null or points to a `blaze_error` this thread may write.
unsafe fn report<T>(error: *mut blaze_error, result: Result<T, Failed>) -> Option<T> {
    let (code, message, value) = match result {
        Ok(value) => (0, std::ptr::null_mut(), Some(value)),
        Err(failed) => (failed.code, c_string(failed.message), None),
    };
    match error.is_null() {
        // SAFETY: not null, and the caller's to write, by this function's contract.
        false => unsafe { error.write(blaze_error { code, message }) },
        // Nobody reads the message: freed here, not leaked.
        // SAFETY: `message` is null or what `c_string` just made, and nothing else holds it.
        true => unsafe { blaze_string_free(message) },
    }
    value
}

/// Runs `body`; a panic in it is the error [`PANIC`].
fn guarded<T>(body: impl FnOnce() -> Result<T, Failed>) -> Result<T, Failed> {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(panic) => {
            let said = panic
                .downcast_ref::<&str>()
                .map(|it| (*it).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic with no message".to_owned());
            Err(Failed { code: PANIC, message: format!("a panic inside blaze-client: {said}") })
        }
    }
}

/// The client `client` points to, when it takes calls.
///
/// # Safety
///
/// `client` is null or a pointer [`blaze_client_connect`] returned that was not closed, used by
/// no other thread during the call.
unsafe fn usable<'a>(client: *mut blaze_client) -> Result<&'a mut blaze_client, Failed> {
    // SAFETY: null or valid and unaliased for the call, by this function's contract.
    let Some(client) = (unsafe { client.as_mut() }) else {
        return Err(argument("the client is NULL"));
    };
    if client.poisoned {
        return Err(Failed {
            code: PANIC,
            message: "a call of this client panicked: close it and connect again".to_owned(),
        });
    }
    Ok(client)
}

/// The bytes at `bytes`, `len` of them.
///
/// # Safety
///
/// `bytes` is null (with any `len`) or points to `len` bytes readable for `'a`.
unsafe fn slice<'a>(bytes: *const u8, len: usize) -> Result<&'a [u8], Failed> {
    if bytes.is_null() {
        return Err(argument("the bytes are NULL"));
    }
    // SAFETY: not null, and `len` readable bytes for `'a`, by this function's contract.
    Ok(unsafe { std::slice::from_raw_parts(bytes, len) })
}

/// Connects to the daemon of `home`, starting it when nothing listens, within `within_ms`.
///
/// # Safety
///
/// `home` is null or a NUL-terminated string readable during the call. `error` is null or
/// points to a `blaze_error` to write (what it held is overwritten, not freed). The pointer
/// returned is null on an error, else the caller's until [`blaze_client_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_client_connect(
    home: *const c_char,
    within_ms: u64,
    error: *mut blaze_error,
) -> *mut blaze_client {
    let connected = guarded(|| {
        let home = match home.is_null() {
            true => None,
            // SAFETY: not null, and NUL-terminated and readable for the call, by contract.
            false => Some(unsafe { CStr::from_ptr(home) }.to_str().map_err(|_| argument("`home` is not UTF-8"))?),
        };
        let client = Client::connect(home.map(Path::new), Duration::from_millis(within_ms))?;
        Ok(Box::into_raw(Box::new(blaze_client { client, poisoned: false })))
    });
    // SAFETY: `error` is null or the caller's to write, by contract.
    unsafe { report(error, connected) }.unwrap_or(std::ptr::null_mut())
}

/// Sends one request of the API and hands each response to `callback` before it returns.
///
/// # Safety
///
/// `client` is a pointer [`blaze_client_connect`] returned, not closed, used by no other thread
/// during the call. `bytes` points to `len` readable bytes. `callback` is null or a function
/// that may be called on this thread with `user` and bytes it must not keep. `error` is null
/// or points to a `blaze_error` to write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_client_request(
    client: *mut blaze_client,
    format: u32,
    bytes: *const u8,
    len: usize,
    within_ms: u64,
    callback: blaze_event_fn,
    user: *mut c_void,
    error: *mut blaze_error,
) -> i32 {
    let ended = guarded(|| {
        // SAFETY: the caller's contract is this function's.
        let (held, bytes) = unsafe { (usable(client)?, slice(bytes, len)?) };
        let refused = |what: String| Failed { code: ASKED, message: what };
        let request: Request = match format {
            JSON => json::request_of(bytes).map_err(|it| refused(it.what))?,
            CBOR => blaze_protocol::cbor::from_slice(bytes)
                .map_err(|it| refused(format!("a request that is not the API's CBOR: {it}")))?,
            other => return Err(argument(&format!("format {other} is neither BLAZE_JSON (0) nor BLAZE_CBOR (1)"))),
        };
        held.poisoned = true;
        let answer = foreign::call(
            &mut held.client,
            |calls, client| calls.begin_request(client, &request, Duration::from_millis(within_ms)),
            |id, response| told(format, callback, user, id, response),
        );
        held.poisoned = false;
        match answer? {
            Answer::Done { ok } => Ok(i32::from(ok)),
            Answer::Outcome(_) | Answer::Rebuilt(_) | Answer::Status(_) => Err(Failed {
                code: PANIC,
                message: "a request was answered as a call: a bug in blaze-client".to_owned(),
            }),
        }
    });
    // SAFETY: `error` is null or the caller's to write, by contract.
    unsafe { report(error, ended) }.unwrap_or(-1)
}

/// One call by name with its options in JSON; the answer as JSON.
///
/// # Safety
///
/// As [`blaze_client_request`]; `method` is a NUL-terminated string readable during the call,
/// and `options` points to `len` readable bytes. The string returned is null on an error, else
/// the caller's until [`blaze_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_client_call(
    client: *mut blaze_client,
    method: *const c_char,
    options: *const u8,
    len: usize,
    callback: blaze_event_fn,
    user: *mut c_void,
    error: *mut blaze_error,
) -> *mut c_char {
    let answered = guarded(|| {
        // SAFETY: the caller's contract is this function's.
        let (held, options) = unsafe { (usable(client)?, slice(options, len)?) };
        if method.is_null() {
            return Err(argument("`method` is NULL"));
        }
        // SAFETY: not null, and NUL-terminated and readable for the call, by contract.
        let method = unsafe { CStr::from_ptr(method) }.to_str().map_err(|_| argument("`method` is not UTF-8"))?;
        held.poisoned = true;
        let answer = foreign::call(
            &mut held.client,
            |calls, client| calls.begin(client, method, options),
            |id, response| told(JSON, callback, user, id, response),
        );
        held.poisoned = false;
        Ok(c_string(answer?.json()))
    });
    // SAFETY: `error` is null or the caller's to write, by contract.
    unsafe { report(error, answered) }.unwrap_or(std::ptr::null_mut())
}

/// Hands `response` of request `id` to `callback` in `format`.
fn told(format: u32, callback: blaze_event_fn, user: *mut c_void, id: u64, response: &Response) {
    let Some(callback) = callback else { return };
    let bytes = match format {
        // The envelope's item: the bytes the daemon's frame held.
        CBOR => blaze_protocol::cbor::to_vec(&Envelope::new(id, response))
            .expect("a response the daemon sent has an encoding"),
        // clone: the envelope written owns its body.
        _ => json::response(&Envelope::new(id, response.clone())).into_bytes(),
    };
    // SAFETY: `callback` is the caller's function, which by `blaze_client_request`'s contract
    // may be called here with `user`; `bytes` lives across the call.
    unsafe { callback(user, bytes.as_ptr(), bytes.len()) };
}

/// Hangs up and frees the client.
///
/// # Safety
///
/// `client` is null or a pointer [`blaze_client_connect`] returned, not closed before and used
/// by no thread now or after.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_client_close(client: *mut blaze_client) {
    if client.is_null() {
        return;
    }
    // SAFETY: made by `Box::into_raw` in `blaze_client_connect` and not freed since, by contract.
    let held = unsafe { Box::from_raw(client) };
    // A panic in a drop is kept from the caller, and nothing is left to say it to.
    let _ = catch_unwind(AssertUnwindSafe(|| held.client.close()));
}

/// Frees a string [`blaze_client_call`] returned, or an error's message.
///
/// # Safety
///
/// `string` is null or a pointer this library returned as a string, not freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_string_free(string: *mut c_char) {
    if !string.is_null() {
        // SAFETY: made by `CString::into_raw` in `c_string` and not freed since, by contract.
        drop(unsafe { CString::from_raw(string) });
    }
}

/// Frees an error's message and sets it to no error.
///
/// # Safety
///
/// `error` is null or points to a `blaze_error` this library wrote (or one zeroed), whose
/// message was not freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_error_free(error: *mut blaze_error) {
    // SAFETY: null or valid and the caller's, by contract.
    if let Some(error) = unsafe { error.as_mut() } {
        // SAFETY: the message is null or this library's string, by contract.
        unsafe { blaze_string_free(error.message) };
        *error = blaze_error { code: 0, message: std::ptr::null_mut() };
    }
}
