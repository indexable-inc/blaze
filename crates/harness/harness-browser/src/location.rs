//! Where the Mac is: macOS's Core Location, asked by the host for the kernel ([`KernelUp::Location`]).
//!
//! The host is the one to ask because it is the one signed app bundle harness has: Core Location answers only an app
//! whose Info.plist says why it wants the place (`NSLocationUsageDescription`, scripts/cef.py), and macOS asks the
//! person about that app by name, once. Chromium's own geolocation is not the way: this embedded build never asks
//! Core Location (a page's `navigator.geolocation` waits until it times out), so the host asks it directly.
//!
//! One `CLLocationManager` on the UI thread (CEF's message loop on macOS is the main run loop, which is where Core
//! Location calls its delegate). A request joins the ones waiting; the manager is asked for one fix
//! (`requestLocation`), after the person's leave when macOS has not been told yet (`requestWhenInUseAuthorization`);
//! the fix, or the refusal, answers everyone waiting. A request waits at most [`WAIT`] and is then told what it was
//! waiting on. Nothing else is tried: no IP lookup, no last known place.
//!
//! Core Location is called by name through the Objective-C runtime, as app_class.rs calls AppKit: three classes and a
//! dozen selectors need no bindings crate. Nothing here is called while the locator is borrowed: Core Location may call
//! the delegate from inside a call into it.
use crate::host::{self, Cmd};
use harness_proto::browser::KernelDown;
use objc2::encode::{Encode, Encoding};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use std::cell::RefCell;
use std::time::Duration;

#[link(name = "CoreLocation", kind = "framework")]
unsafe extern "C" {}

/// How long one request waits for a fix. Long enough for the person to answer macOS's question the first time.
pub const WAIT: Duration = Duration::from_secs(60);

/// `CLAuthorizationStatus`: not asked yet; then the two refusals; anything above is leave given.
const NOT_DETERMINED: i32 = 0;
const RESTRICTED: i32 = 1;
const DENIED: i32 = 2;
/// `kCLErrorLocationUnknown` and `kCLErrorDenied` (CLError.h).
const ERR_UNKNOWN: isize = 0;
const ERR_DENIED: isize = 1;

const SETTINGS: &str = "System Settings > Privacy & Security > Location Services";

/// `CLLocationCoordinate2D`: degrees.
#[repr(C)]
#[derive(Clone, Copy)]
struct Coordinate {
    latitude: f64,
    longitude: f64,
}

// SAFETY: CLLocationCoordinate2D is two doubles, latitude then longitude (CLLocation.h).
unsafe impl Encode for Coordinate {
    const ENCODING: Encoding = Encoding::Struct("CLLocationCoordinate2D", &[f64::ENCODING, f64::ENCODING]);
}

struct Locator {
    manager: Retained<AnyObject>,
    /// The manager holds its delegate weakly: this keeps it.
    _delegate: Retained<AnyObject>,
    /// Who waits for the fix: (connection, request).
    waiting: Vec<(u64, u64)>,
    /// The person has been asked (once a host: macOS asks once, then the answer is in System Settings).
    asked: bool,
}

thread_local! {
    static LOCATOR: RefCell<Option<Locator>> = const { RefCell::new(None) };
}

fn manager() -> Result<Retained<AnyObject>, String> {
    if let Some(m) = LOCATOR.with_borrow(|l| l.as_ref().map(|l| l.manager.clone())) {
        return Ok(m);
    }
    let cls = AnyClass::get(c"CLLocationManager").ok_or("the Objective-C class CLLocationManager is not loaded (Core Location is not linked)")?;
    let ns_object = AnyClass::get(c"NSObject").ok_or("the Objective-C class NSObject is not loaded")?;
    let mut b = ClassBuilder::new(c"HarnessBrowserLocationDelegate", ns_object).ok_or("the Objective-C class HarnessBrowserLocationDelegate is already registered")?;
    // SAFETY: each function's signature is its selector's in CLLocationManagerDelegate (void; the manager, then an
    // NSArray of CLLocation, an NSError, or nothing).
    unsafe {
        b.add_method(sel!(locationManager:didUpdateLocations:), did_update as extern "C-unwind" fn(_, _, _, _));
        b.add_method(sel!(locationManager:didFailWithError:), did_fail as extern "C-unwind" fn(_, _, _, _));
        b.add_method(sel!(locationManagerDidChangeAuthorization:), did_change as extern "C-unwind" fn(_, _, _));
    }
    let delegate_cls = b.register();
    // SAFETY: +new on NSObject's subclass and on CLLocationManager, on the main thread (where a manager must be made:
    // its delegate is called on the run loop of the thread that made it).
    let (delegate, manager): (Retained<AnyObject>, Retained<AnyObject>) = unsafe { (msg_send![delegate_cls, new], msg_send![cls, new]) };
    // SAFETY: -setDelegate: with a live object the locator keeps; -setDesiredAccuracy: takes a double
    // (kCLLocationAccuracyBest is -1).
    unsafe {
        let _: () = msg_send![&*manager, setDelegate: &*delegate];
        let _: () = msg_send![&*manager, setDesiredAccuracy: -1.0f64];
    }
    LOCATOR.with_borrow_mut(|l| *l = Some(Locator { manager: manager.clone(), _delegate: delegate, waiting: Vec::new(), asked: false })); // alloc: once a host
    Ok(manager)
}

/// The kernel's request `req` on connection `conn`: answered with the fix or the refusal, within [`WAIT`].
pub fn request(conn: u64, req: u64) {
    let manager = match manager() {
        Ok(m) => m,
        Err(e) => return host::answer(conn, &KernelDown::Error { req: Some(req), error: format!("where this Mac is: {e}") }),
    };
    let first = LOCATOR.with_borrow_mut(|l| {
        let l = l.as_mut().expect("manager() made the locator");
        l.waiting.push((conn, req));
        l.waiting.len() == 1
    });
    let spawned = std::thread::Builder::new().name("harness-browser.location-wait".into()).spawn(move || {
        // Waits for: request `req`'s bound to pass (WAIT). The UI thread then answers it if no fix has.
        std::thread::sleep(WAIT);
        host::post(Cmd::LocationWaited { conn, req });
    });
    if let Err(e) = spawned {
        return fail(format!("spawning the thread that bounds the wait: {e}"));
    }
    // One fix answers everyone: only the first of those waiting asks for it.
    if first {
        ask(&manager);
    }
}

/// Ask for the fix, or for the person's leave first, or refuse everyone waiting: by what macOS has been told.
fn ask(manager: &AnyObject) {
    // SAFETY: +locationServicesEnabled and -authorizationStatus (an int enum) on a live manager.
    let (on, status): (bool, i32) = unsafe { (msg_send![manager.class(), locationServicesEnabled], msg_send![manager, authorizationStatus]) };
    if !on {
        return fail(format!("Location Services is off for this Mac: turn it on in {SETTINGS}"));
    }
    match status {
        NOT_DETERMINED => {
            let asked = LOCATOR.with_borrow_mut(|l| l.as_mut().is_some_and(|l| std::mem::replace(&mut l.asked, true)));
            if !asked {
                // SAFETY: -requestWhenInUseAuthorization on a live manager. macOS shows its question; the answer comes
                // as locationManagerDidChangeAuthorization:, which asks again.
                let _: () = unsafe { msg_send![manager, requestWhenInUseAuthorization] };
            }
        }
        RESTRICTED => fail(format!("this Mac's Location Services is restricted (a profile or parental control): Harness Browser may not use it ({SETTINGS})")),
        DENIED => fail(format!("Harness Browser is not allowed to use Location Services: allow it in {SETTINGS}")),
        // SAFETY: -requestLocation on a live, authorized manager: one fix, then locationManager:didUpdateLocations: or
        // locationManager:didFailWithError:.
        _ => unsafe { msg_send![manager, requestLocation] },
    }
}

fn waiting() -> Vec<(u64, u64)> {
    LOCATOR.with_borrow_mut(|l| l.as_mut().map(|l| std::mem::take(&mut l.waiting)).unwrap_or_default())
}

/// Refuse everyone waiting.
fn fail(why: String) {
    for (conn, req) in waiting() {
        host::answer(conn, &KernelDown::Error { req: Some(req), error: format!("where this Mac is: {why}") });
    }
}

/// Request `req`'s bound passed ([`Cmd::LocationWaited`]): if it still waits, it is told what it waited on.
pub fn waited(conn: u64, req: u64) {
    let (still, manager) = LOCATOR.with_borrow_mut(|l| {
        let Some(l) = l.as_mut() else { return (false, None) };
        let n = l.waiting.len();
        l.waiting.retain(|w| *w != (conn, req));
        (l.waiting.len() != n, Some(l.manager.clone()))
    });
    let (true, Some(manager)) = (still, manager) else { return };
    // SAFETY: -authorizationStatus on a live manager.
    let status: i32 = unsafe { msg_send![&*manager, authorizationStatus] };
    let secs = WAIT.as_secs();
    let why = if status == NOT_DETERMINED {
        // Two causes look the same from here. locationd serves one running process per app (bundle id): while another
        // Harness Browser is connected to it (another harness daemon's host), this one's registration is held back
        // and macOS never asks ("another valid client with the same bundle id is connected", in locationd's log).
        format!(
            "macOS has not said whether Harness Browser may use Location Services, {secs} s after being asked: answer its question (or allow it in {SETTINGS}); \
             if it asked nothing, another Harness Browser is running (another harness daemon's: `pgrep -fl 'MacOS/Harness Browser$'`) and macOS serves only one, so stop the other"
        )
    } else {
        format!("Core Location gave no fix within {secs} s (Wi-Fi off, or no network to place it by?)")
    };
    host::answer(conn, &KernelDown::Error { req: Some(req), error: format!("where this Mac is: {why}") });
}

extern "C-unwind" fn did_update(_this: &AnyObject, _cmd: Sel, _manager: *mut AnyObject, locations: *mut AnyObject) {
    if locations.is_null() {
        return;
    }
    // SAFETY: an NSArray of CLLocation, newest last (the delegate's contract); its getters return a struct of two
    // doubles, doubles, and an NSDate.
    let fix = unsafe {
        let last: Option<Retained<AnyObject>> = msg_send![&*locations, lastObject];
        last.map(|loc| {
            let at: Coordinate = msg_send![&*loc, coordinate];
            let accuracy: f64 = msg_send![&*loc, horizontalAccuracy];
            let altitude: f64 = msg_send![&*loc, altitude];
            let vertical: f64 = msg_send![&*loc, verticalAccuracy];
            let when: Retained<AnyObject> = msg_send![&*loc, timestamp];
            let secs: f64 = msg_send![&*when, timeIntervalSince1970];
            (at, accuracy, altitude, vertical, secs)
        })
    };
    // A negative accuracy is Core Location's "this value is not valid".
    let Some((at, accuracy, altitude, vertical, secs)) = fix.filter(|f| f.1 >= 0.0) else {
        return fail("Core Location answered with no valid fix".into());
    };
    for (conn, req) in waiting() {
        host::answer(
            conn,
            &KernelDown::Location {
                req,
                latitude: at.latitude,
                longitude: at.longitude,
                accuracy_m: accuracy,
                altitude_m: (vertical > 0.0).then_some(altitude),
                at_ms: (secs * 1000.0).max(0.0) as u64,
            },
        );
    }
}

extern "C-unwind" fn did_fail(_this: &AnyObject, _cmd: Sel, _manager: *mut AnyObject, error: *mut AnyObject) {
    // SAFETY: an NSError (the delegate's contract); -code is an NSInteger.
    let code: isize = if error.is_null() { -1 } else { unsafe { msg_send![&*error, code] } };
    fail(match code {
        ERR_UNKNOWN => "Core Location could not place this Mac right now (Wi-Fi off, or no network to place it by?)".into(),
        ERR_DENIED => format!("Harness Browser is not allowed to use Location Services: allow it in {SETTINGS}"),
        n => format!("Core Location failed with CLError {n}"),
    });
}

extern "C-unwind" fn did_change(_this: &AnyObject, _cmd: Sel, manager: *mut AnyObject) {
    // Called once when the manager is made, and again when the person answers: only someone waiting makes it ask.
    let anyone = LOCATOR.with_borrow(|l| l.as_ref().is_some_and(|l| !l.waiting.is_empty()));
    if anyone && !manager.is_null() {
        // SAFETY: the live manager Core Location handed its delegate.
        ask(unsafe { &*manager });
    }
}
