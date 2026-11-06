//! The NSApplication CEF needs on macOS: `NSApp` must be an instance of a class that conforms to
//! `CrAppControlProtocol` and `CrAppProtocol` (`isHandlingSendEvent` / `setHandlingSendEvent:`, kept true while
//! `sendEvent:` runs) before `cef_initialize`, or Chromium's message pump refuses it. cefclient defines it in Objective-C;
//! here it is one class registered at run time with objc2's `ClassBuilder` (no AppKit bindings needed), made the shared
//! application before anything else touches `NSApp`.
//!
//! The protocols are the framework's (it references them, so they exist once it is loaded): install after loading it.
//! `CefAppProtocol` itself is not among them: cef_application_mac.h declares it, but only a client compiled against
//! that header registers it, and the framework never asks for it (its binary names `CrAppProtocol` and
//! `CrAppControlProtocol` only; Chromium's pump checks `CrAppProtocol`). So the class conforms to those two.
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// `sendEvent:` is running (main thread only; an atomic so the static needs no lock).
static HANDLING: AtomicBool = AtomicBool::new(false);
static SUPER: OnceLock<&'static AnyClass> = OnceLock::new();

extern "C-unwind" fn is_handling(_this: &AnyObject, _cmd: Sel) -> Bool {
    Bool::new(HANDLING.load(Ordering::Relaxed))
}

extern "C-unwind" fn set_handling(_this: &AnyObject, _cmd: Sel, on: Bool) {
    HANDLING.store(on.as_bool(), Ordering::Relaxed);
}

extern "C-unwind" fn send_event(this: &AnyObject, _cmd: Sel, event: *mut AnyObject) {
    let was = HANDLING.swap(true, Ordering::Relaxed);
    let sup = SUPER.get().expect("app_class::install set the superclass before registering sendEvent:");
    // SAFETY: NSApplication's own -sendEvent: with the event AppKit handed us.
    let _: () = unsafe { msg_send![super(this, *sup), sendEvent: event] };
    HANDLING.store(was, Ordering::Relaxed);
}

/// Register `HarnessBrowserApplication` and make it the shared application.
///
/// # Errors
/// NSApplication or one of CEF's protocols not found (the framework is not loaded), the class name taken, or
/// `NSApp` already something else.
pub fn install() -> Result<(), String> {
    let ns_app = AnyClass::get(c"NSApplication").ok_or("the Objective-C class NSApplication is not loaded (AppKit is not linked)")?;
    let _ = SUPER.set(ns_app);
    let mut b = ClassBuilder::new(c"HarnessBrowserApplication", ns_app).ok_or("the Objective-C class HarnessBrowserApplication is already registered")?;
    // SAFETY: each function's signature is the selector's (BOOL getter, BOOL setter, void with one object).
    unsafe {
        b.add_method(sel!(isHandlingSendEvent), is_handling as extern "C-unwind" fn(_, _) -> _);
        b.add_method(sel!(setHandlingSendEvent:), set_handling as extern "C-unwind" fn(_, _, _));
        b.add_method(sel!(sendEvent:), send_event as extern "C-unwind" fn(_, _, _));
    }
    for name in [c"CrAppProtocol", c"CrAppControlProtocol"] {
        let p = AnyProtocol::get(name).ok_or_else(|| {
            format!("the Objective-C protocol {} is not registered: the Chromium Embedded Framework must be loaded before app_class::install", name.to_string_lossy())
        })?;
        b.add_protocol(p);
    }
    let cls = b.register();
    // SAFETY: +sharedApplication on our NSApplication subclass, on the main thread, before anything made NSApp.
    let app: *mut AnyObject = unsafe { msg_send![cls, sharedApplication] };
    if app.is_null() {
        return Err("[HarnessBrowserApplication sharedApplication] returned nil".into());
    }
    // SAFETY: a live object just returned.
    let is_ours: bool = unsafe { msg_send![&*app, isKindOfClass: cls] };
    if !is_ours {
        return Err("NSApp was made before app_class::install, so it is no HarnessBrowserApplication and CEF would refuse it".into());
    }
    Ok(())
}
