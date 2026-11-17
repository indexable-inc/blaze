//! Places near a point: Apple Maps' own search (MapKit's `MKLocalSearch`), asked by the host for the kernel
//! ([`KernelUp::Places`]).
//!
//! "Coffee near me" is one question to the map the person already uses, answered in under a second with names,
//! addresses, phones and coordinates; without it an agent scrapes search pages for the same list. MapKit needs no key
//! from a native process. The host asks because it is the native process the kernel already talks to (location.rs).
//!
//! One search a request, on the UI thread (MapKit calls its completion handler on the main queue). The search object is
//! kept until its handler ran; MapKit always calls it, with places or an error (its own network timeout). The kernel
//! bounds its wait (cdp.ts `HOST_MS`). MapKit is called by name through the Objective-C runtime, as location.rs calls
//! Core Location.
use crate::host;
use block2::RcBlock;
use harness_proto::browser::{KernelDown, Place};
use objc2::encode::{Encode, Encoding};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_foundation::NSString;
use std::cell::RefCell;
use std::collections::HashMap;

#[link(name = "MapKit", kind = "framework")]
unsafe extern "C" {}

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

/// `MKCoordinateSpan`: degrees of latitude and of longitude.
#[repr(C)]
#[derive(Clone, Copy)]
struct Span {
    latitude: f64,
    longitude: f64,
}

// SAFETY: MKCoordinateSpan is an unnamed struct of two doubles (MKGeometry.h).
unsafe impl Encode for Span {
    const ENCODING: Encoding = Encoding::Struct("?", &[f64::ENCODING, f64::ENCODING]);
}

/// `MKCoordinateRegion`: a centre and the span around it.
#[repr(C)]
#[derive(Clone, Copy)]
struct Region {
    center: Coordinate,
    span: Span,
}

// SAFETY: MKCoordinateRegion is an unnamed struct of a centre and a span (MKGeometry.h).
unsafe impl Encode for Region {
    const ENCODING: Encoding = Encoding::Struct("?", &[Coordinate::ENCODING, Span::ENCODING]);
}

/// Metres in a degree of latitude.
const DEGREE_M: f64 = 111_320.0;
/// The most places one answer carries.
pub const MAX_PLACES: usize = 50;

thread_local! {
    /// The searches under way, by (connection, request): MapKit does not keep a search alive itself.
    static SEARCHES: RefCell<HashMap<(u64, u64), Retained<AnyObject>>> = RefCell::new(HashMap::new());
}

fn refuse(conn: u64, req: u64, why: &str) {
    host::answer(conn, &KernelDown::Error { req: Some(req), error: format!("places: {why}") });
}

/// An `NSString` property of `obj` as text; empty when it has none.
///
/// # Safety
/// `obj` is a live object whose `sel` getter returns an NSString or nil.
unsafe fn text(obj: &AnyObject, sel: objc2::runtime::Sel) -> String {
    // A getter this object does not have is no text, never an exception: an unrecognised selector would end the host.
    // SAFETY: -respondsToSelector: on a live object.
    let has: bool = unsafe { msg_send![obj, respondsToSelector: sel] };
    if !has {
        eprintln!("harness-browser: places: {:?} has no -{}", obj.class().name(), sel.name().to_string_lossy());
        return String::new();
    }
    // SAFETY: the caller's contract.
    let s: Option<Retained<NSString>> = unsafe { msg_send![obj, performSelector: sel] };
    s.map(|s| s.to_string()).unwrap_or_default()
}

/// The kernel's request `req` on connection `conn`: the places Apple Maps finds for `query` within `radius_m` of the
/// point, at most `limit` (and [`MAX_PLACES`]), as Maps ranks them.
pub fn request(conn: u64, req: u64, query: &str, latitude: f64, longitude: f64, radius_m: f64, limit: u32) {
    if query.trim().is_empty() {
        return refuse(conn, req, "the query is empty");
    }
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) || !(radius_m > 0.0) {
        return refuse(conn, req, &format!("near {latitude}, {longitude} within {radius_m} m is no place on Earth"));
    }
    let (Some(request_cls), Some(search_cls)) = (AnyClass::get(c"MKLocalSearchRequest"), AnyClass::get(c"MKLocalSearch")) else {
        return refuse(conn, req, "MapKit's MKLocalSearch is not loaded");
    };
    let span = Span { latitude: 2.0 * radius_m / DEGREE_M, longitude: 2.0 * radius_m / (DEGREE_M * latitude.to_radians().cos().max(0.01)) };
    let region = Region { center: Coordinate { latitude, longitude }, span };
    // SAFETY: +new, the request's setters (an NSString, an MKCoordinateRegion by value), and MKLocalSearch's
    // designated initialiser with that request; all on the main thread.
    let search: Retained<AnyObject> = unsafe {
        let r: Retained<AnyObject> = msg_send![request_cls, new];
        let _: () = msg_send![&*r, setNaturalLanguageQuery: &*NSString::from_str(query)];
        let _: () = msg_send![&*r, setRegion: region];
        let alloc: objc2::rc::Allocated<AnyObject> = msg_send![search_cls, alloc];
        msg_send![alloc, initWithRequest: &*r]
    };
    SEARCHES.with_borrow_mut(|s| s.insert((conn, req), search.clone()));
    let limit = (limit as usize).clamp(1, MAX_PLACES);
    let query = query.to_owned();
    let done = RcBlock::new(move |response: *mut AnyObject, error: *mut AnyObject| {
        SEARCHES.with_borrow_mut(|s| s.remove(&(conn, req)));
        if response.is_null() {
            if error.is_null() {
                return refuse(conn, req, "Apple Maps' search gave neither places nor an error");
            }
            // SAFETY: an NSError when there is no response (the handler's contract): -code is an NSInteger, -domain an
            // NSString.
            let (code, domain): (isize, String) = unsafe { (msg_send![&*error, code], text(&*error, objc2::sel!(domain))) };
            // MKErrorPlacemarkNotFound (4): nothing matched, which is an answer, not a failure.
            if domain == "MKErrorDomain" && code == 4 {
                return host::answer(conn, &KernelDown::Places { req, places: Vec::new() });
            }
            let why = match (domain.as_str(), code) {
                ("MKErrorDomain", 2) => "a network failure".to_owned(),
                ("MKErrorDomain", 3) => "Apple's servers are throttling this Mac's searches (wait a little)".to_owned(),
                ("NSURLErrorDomain", -1001) => "the request to Apple's servers timed out (a slow network)".to_owned(),
                ("NSURLErrorDomain", -1009) => "this Mac is offline".to_owned(),
                _ => format!("{domain} error {code}"),
            };
            return refuse(conn, req, &format!("Apple Maps' search for {query:?} failed: {why}"));
        }
        // SAFETY: an MKLocalSearchResponse; mapItems is an NSArray of MKMapItem, whose getters return NSStrings, an
        // NSURL and an MKPlacemark (each possibly nil), and the placemark's coordinate a struct of two doubles.
        let places = unsafe {
            let items: Retained<AnyObject> = msg_send![&*response, mapItems];
            let n: usize = msg_send![&*items, count];
            let mut out = Vec::with_capacity(n.min(limit)); // alloc: one answer's places
            for i in 0..n.min(limit) {
                let item: Retained<AnyObject> = msg_send![&*items, objectAtIndex: i];
                let mark: Option<Retained<AnyObject>> = msg_send![&*item, placemark];
                let Some(mark) = mark else { continue };
                let at: Coordinate = msg_send![&*mark, coordinate];
                let url: Option<Retained<AnyObject>> = msg_send![&*item, url];
                // MKMapItem.identifier (macOS 15): an MKMapItemIdentifier, or nil; its identifierString is the id's text
                // (Swift's rawValue; Objective-C has no selector of that name).
                let ident: Option<Retained<AnyObject>> = msg_send![&*item, identifier];
                out.push(Place {
                    id: ident.map(|i| text(&i, objc2::sel!(identifierString))).unwrap_or_default(),
                    name: text(&item, objc2::sel!(name)),
                    latitude: at.latitude,
                    longitude: at.longitude,
                    address: text(&mark, objc2::sel!(title)),
                    phone: text(&item, objc2::sel!(phoneNumber)),
                    url: url.map(|u| text(&u, objc2::sel!(absoluteString))).unwrap_or_default(),
                    category: text(&item, objc2::sel!(pointOfInterestCategory)).trim_start_matches("MKPOICategory").to_owned(),
                });
            }
            out
        };
        host::answer(conn, &KernelDown::Places { req, places });
    });
    // SAFETY: -startWithCompletionHandler: with a block of (MKLocalSearchResponse*, NSError*), called once on the main
    // queue; the search is kept in SEARCHES until then.
    let _: () = unsafe { msg_send![&*search, startWithCompletionHandler: &*done] };
}
