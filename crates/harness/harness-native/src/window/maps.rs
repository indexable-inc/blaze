//! Apple's map over a cell's `Map` card (src/app/maps.rs): one live `MKMapView` per card the frame painted, a view of
//! the main window laid over the card's plate, so the person pans and zooms it in place with MapKit's own gestures.
//!
//! Where. The views sit in the main view's superview (the window's content view), above the Metal layer the app draws
//! in. Each card is three views: `cut`, a plain view at the part of the card its clips leave to see (the transcript's
//! scroll, the pane), so a card partly scrolled away shows only that part; in it `plate`, at the card's own box, its
//! layer rounded to the card's corners; in that the map, as large as the card. Every frame [`Maps::sync`] moves `cut` and `plate`
//! to where layout put the card (view points at the app's zoom, turned into the content view's points), makes the views
//! of cards new to the frame and removes those of cards gone from it (a row the lazy list culled, a chat switched
//! away), and hides a card with nothing left to see. The map never moves against its card.
//!
//! What it shows. A card's places are its `spec`; while that is the same the map is left as the person moved it, and a
//! new one sets its pins again and looks again ([`Places::region`]), never animated. A place is an `MKPointAnnotation`
//! (its label the title, its detail the subtitle), drawn as MapKit's own marker; one with an `href` has a callout with
//! an Open button, which opens the page as a chat's link does ([`App::open_target`](crate::app::App)). The person's
//! place (`here`) is an annotation of its own drawn as a blue dot ringed in white: never `showsUserLocation`, which
//! would have this app ask for the person's location (the kernel already has the fix it was given).
//!
//! Apple's place card. A place that says which place of Apple Maps it is (`place`, an `MKMapItem`'s identifier) is
//! asked of MapKit (`MKMapItemRequest`); until the answer it is the plain pin, and with it the pin is replaced by an
//! `MKMapItemAnnotation`, which MapKit draws as that place's own marker (its icon and colour). Pressing one shows
//! Apple's place card (`MKMapItemDetailViewController`: photos, hours, rating, address, Open in Maps) in a popover
//! pointing at the pin. The card is a popover, not MapKit's selection accessory (`mapView:selectionAccessoryForAnnotation:`),
//! because an accessory is a subview of the map and is cut by the card: the full one is 320 by 584 points, taller than
//! a Map card, and the automatic style falls back to the compact one (253 by 85, no photos; measured on macOS 27).
//! The points of interest Apple draws (a cafe, a park) are pressable for the same card: see [`POI_SELECTORS`]. One
//! card at a time; it closes with a press anywhere else (a transient popover), with its own close button, and when
//! its Map card moves, changes its places or leaves the frame. Nothing is animated.
//!
//! Appearance: each card's views take the appearance its card is drawn in (the window's, or its `Scheme`'s), so the map
//! is Apple's dark or light one with the app.
//!
//! Keys stay the app's: `MKMapView` refuses first responder on macOS, and should anything inside a map take it, the
//! next frame hands it back to the main view (the composer keeps the typing). A place card is a window of its own
//! (the popover's), outside every map, so it is never handed back from; while it is up it has the keys, and the main
//! window has them again, its first responder as it was, when it closes.
//!
//! Limits. A view of AppKit's is above everything the app draws in the main window's Metal layer: a toast, a sheet, a
//! dialog or the lines over the composer that reach over a map are under it (menus, popovers and hover cards are
//! windows of their own, overlay.rs, and stay above). The edge of a scroll cuts a map hard where words thin out. The
//! wheel over a map is the map's. A Map inside a windowed overlay has no map (its card stays an empty plate).
//!
//! MapKit by name: the classes are declared here (`extern_class!`) and messaged with `msg_send!`, the structs with
//! their Objective-C encodings (`@encode`, checked against the framework on macOS 27), so the app takes no dependency
//! for them; the framework is linked below.
use super::{HarnessView, with};
use crate::app::{MapCard, Places};
use block2::RcBlock;
use objc2::encode::{Encode, Encoding};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, extern_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSButton, NSColor, NSPopover, NSPopoverBehavior, NSResponder, NSView, NSViewController, NSWindow,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRectEdge, NSSize, NSString, ns_string};
use std::cell::{Cell, RefCell};

#[link(name = "MapKit", kind = "framework")]
unsafe extern "C" {}

/// The blue dot's width (points) and its white ring's.
const DOT: f64 = 16.0;
const DOT_RING: f64 = 3.0;

/// A place card's size (points): what MapKit's own full place card takes (its callout, measured on macOS 27).
const PLACE_CARD: (f64, f64) = (320.0, 584.0);
/// The box around a point of interest's own mark that its place card points at (points).
const LABEL_BOX: f64 = 24.0;

/// What pressing Apple's own points of interest needs of `MKMapView`, all private on macOS (the public
/// `selectableMapFeatures` and `MKMapFeatureAnnotation` are iOS's only; macOS 27 has neither): `_setCanSelectPOIs:`
/// (a BOOL) lets a press select a label Apple drew, which the delegate hears as `mapView:didSelectLabelMarker:` with
/// VectorKit's `VKLabelMarker`; `_selectedLabelMarker` is the one selected (or nil) and `_deselectLabelMarkerAnimated:`
/// (a BOOL) lets it go. A map lacking any of them leaves its points of interest unpressable and says so
/// (`pois_pressable`, and the test `pois_are_pressable`).
const POI_SELECTORS: [&core::ffi::CStr; 3] = [c"_setCanSelectPOIs:", c"_selectedLabelMarker", c"_deselectLabelMarkerAnimated:"];
/// What is read of a `VKLabelMarker` (private, VectorKit): whether it is a place of Apple Maps (`hasBusinessID`, a
/// BOOL), which (`businessID`, an unsigned 64-bit number: the place's `MKMapItemIdentifier` is "I" and its 16 hex
/// digits, upper case) and where its mark is (`coordinate`, `{?=dd}`: latitude, longitude).
const LABEL_SELECTORS: [&core::ffi::CStr; 3] = [c"hasBusinessID", c"businessID", c"coordinate"];

/// Whether instances of the class named `class` answer every selector of `names`.
fn answers(class: &core::ffi::CStr, names: &[&core::ffi::CStr]) -> bool {
    AnyClass::get(class).is_some_and(|c| names.iter().all(|n| c.responds_to(Sel::register(n))))
}

/// Whether this system's MapKit lets Apple's points of interest be pressed ([`POI_SELECTORS`], [`LABEL_SELECTORS`]).
fn pois_pressable() -> bool {
    answers(c"MKMapView", &POI_SELECTORS) && answers(c"VKLabelMarker", &LABEL_SELECTORS)
}

/// `CLLocationCoordinate2D`: degrees.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // read by MapKit, not here
struct Coordinate {
    latitude: f64,
    longitude: f64,
}

// SAFETY: CLLocationCoordinate2D is two doubles, latitude then longitude (CLLocation.h).
unsafe impl Encode for Coordinate {
    const ENCODING: Encoding = Encoding::Struct("CLLocationCoordinate2D", &[f64::ENCODING, f64::ENCODING]);
}

/// `MKCoordinateSpan`: degrees of latitude, then of longitude.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // read by MapKit, not here
struct Span {
    latitude_delta: f64,
    longitude_delta: f64,
}

// SAFETY: MKCoordinateSpan is an unnamed struct of two doubles (MKGeometry.h; `@encode` gives `{?=dd}`).
unsafe impl Encode for Span {
    const ENCODING: Encoding = Encoding::Struct("?", &[f64::ENCODING, f64::ENCODING]);
}

/// `MKCoordinateRegion`: a centre and the span around it.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // read by MapKit, not here
struct Region {
    center: Coordinate,
    span: Span,
}

// SAFETY: MKCoordinateRegion is an unnamed struct of a CLLocationCoordinate2D and an MKCoordinateSpan (MKGeometry.h;
// `@encode` gives `{?={CLLocationCoordinate2D=dd}{?=dd}}`).
unsafe impl Encode for Region {
    const ENCODING: Encoding = Encoding::Struct("?", &[Coordinate::ENCODING, Span::ENCODING]);
}

/// A `VKLabelMarker`'s `coordinate`: latitude, then longitude, degrees.
#[repr(C)]
#[derive(Clone, Copy)]
struct LabelAt {
    latitude: f64,
    longitude: f64,
}

// SAFETY: `-[VKLabelMarker coordinate]` returns an unnamed struct of two doubles (`method_getTypeEncoding` gives
// `{?=dd}16@0:8` on macOS 27), latitude first (checked against the labels of a known map).
unsafe impl Encode for LabelAt {
    const ENCODING: Encoding = Encoding::Struct("?", &[f64::ENCODING, f64::ENCODING]);
}

extern_class!(
    /// MapKit's map view (MKMapView.h): an NSView on macOS.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    struct MKMapView;
);

extern_class!(
    /// What MapKit draws an annotation as (MKAnnotationView.h): an NSView on macOS.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    struct MKAnnotationView;
);

extern_class!(
    /// MapKit's balloon marker (MKMarkerAnnotationView.h).
    #[unsafe(super(MKAnnotationView, NSView, NSResponder, NSObject))]
    struct MKMarkerAnnotationView;
);

extern_class!(
    /// MKPointAnnotation's superclass (MKShape.h): the title and subtitle.
    #[unsafe(super(NSObject))]
    struct MKShape;
);

extern_class!(
    /// A titled point of the map (MKPointAnnotation.h).
    #[unsafe(super(MKShape, NSObject))]
    struct MKPointAnnotation;
);

extern_class!(
    /// A place of Apple Maps as an annotation (MKMapItemAnnotation.h, macOS 15): MapKit draws it as the place's own
    /// marker.
    #[unsafe(super(NSObject))]
    struct MKMapItemAnnotation;
);

/// What a pin of ours is beyond its point and words: the person's place (drawn as the dot), or a place with the page
/// its callout opens (empty: none) and the place of Apple Maps it is (empty: unknown).
struct PinIvars {
    here: bool,
    href: String,
    place: String,
}

define_class!(
    // SAFETY: MKPointAnnotation has no subclassing requirements; no Drop.
    #[unsafe(super(MKPointAnnotation, MKShape, NSObject))]
    #[name = "HarnessMapPin"]
    #[ivars = PinIvars]
    struct MapPin;

    unsafe impl NSObjectProtocol for MapPin {}
);

impl MapPin {
    fn new(at: [f64; 2], title: &str, subtitle: &str, here: bool, href: &str, place: &str) -> Retained<Self> {
        let this = Self::alloc().set_ivars(PinIvars { here, href: href.to_owned(), place: place.to_owned() });
        // SAFETY: init is MKPointAnnotation's designated initialiser.
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        // SAFETY: MKPointAnnotation's own setters: a CLLocationCoordinate2D by value, and NSStrings it copies.
        unsafe {
            let _: () = msg_send![&*this, setCoordinate: Coordinate { latitude: at[0], longitude: at[1] }];
            if !title.is_empty() {
                let _: () = msg_send![&*this, setTitle: &*NSString::from_str(title)];
            }
            if !subtitle.is_empty() {
                let _: () = msg_send![&*this, setSubtitle: &*NSString::from_str(subtitle)];
            }
        }
        this
    }
}

/// What a place card is of: a place's pin, or a point of interest Apple drew (its `businessID`).
enum Shown {
    Pin(Retained<MKMapItemAnnotation>),
    Label(u64),
}

/// The place card on screen: the map it is over, its popover, and what it is of.
struct Open {
    map: Retained<MKMapView>,
    popover: Retained<NSPopover>,
    of: Shown,
}

/// The one place card (None: none is up).
struct DelegateIvars {
    open: RefCell<Option<Open>>,
}

/// What a place asked of Apple Maps is for: the pin of card `card` that named it, or the point of interest
/// `business` pressed on `map`.
enum Want {
    Pin { card: u64 },
    Label { map: Retained<MKMapView>, business: u64 },
}

thread_local! {
    /// The places being asked of Apple Maps, by token: MapKit does not keep an `MKMapItemRequest` alive itself. Each
    /// is here until its handler ran (MapKit always calls it, with the place or an error).
    static REQUESTS: RefCell<Vec<(u64, Retained<AnyObject>)>> = const { RefCell::new(Vec::new()) }; // alloc: the few requests under way, each until its answer
    /// The next request's token.
    static NEXT: Cell<u64> = const { Cell::new(0) };
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HarnessMapDelegate"]
    #[ivars = DelegateIvars]
    struct MapDelegate;

    unsafe impl NSObjectProtocol for MapDelegate {}

    impl MapDelegate {
        /// MKMapViewDelegate's: the person's place as the blue dot, a place with a page as a marker whose callout has an
        /// Open button, and any other annotation as MapKit's own (nil).
        #[unsafe(method_id(mapView:viewForAnnotation:))]
        fn view_for(&self, _map: &MKMapView, annotation: &AnyObject) -> Option<Retained<NSView>> {
            // In a method of its own: an early return cannot leave the macro's method body.
            self.annotation_view(annotation)
        }

        /// MKMapViewDelegate's: a pin was pressed. A place of Apple Maps shows its place card.
        #[unsafe(method(mapView:didSelectAnnotationView:))]
        fn did_select(&self, map: &MKMapView, view: &MKAnnotationView) {
            self.pin_selected(map, view)
        }

        /// MKMapViewDelegate's: a pin was let go (a press elsewhere on the map). Its place card goes.
        #[unsafe(method(mapView:didDeselectAnnotationView:))]
        fn did_deselect(&self, map: &MKMapView, _view: &MKAnnotationView) {
            self.pin_deselected(map)
        }

        /// MapKit's own delegate call (private, [`POI_SELECTORS`]): a label Apple drew was pressed.
        #[unsafe(method(mapView:didSelectLabelMarker:))]
        fn did_select_label(&self, map: &MKMapView, marker: &AnyObject) {
            self.label_selected(map, marker)
        }

        /// NSPopoverDelegate's: a place card closed (a press outside it).
        #[unsafe(method(popoverDidClose:))]
        fn popover_did_close(&self, note: &NSNotification) {
            self.popover_gone(note)
        }

        /// MKMapItemDetailViewControllerDelegate's: the place card's own close button.
        #[unsafe(method(mapItemDetailViewControllerDidFinish:))]
        fn card_finished(&self, _card: &NSViewController) {
            self.dismiss()
        }

        /// [`MapDelegate::close_soon_on`]'s: close this place card, if it is still the one up.
        #[unsafe(method(closePlaceCard:))]
        fn close_place_card(&self, popover: &NSPopover) {
            self.close_if(popover)
        }

        /// A callout's Open button: its place's page opens as a chat's link does, in the browser pane.
        #[unsafe(method(openPin:))]
        fn open_pin(&self, sender: &NSView) {
            let Some(href) = sender.toolTip() else { return };
            let href = href.to_string();
            with(|st| {
                st.app.open_target(&href, false);
                st.link.setPaused(false);
            });
        }
    }
);

impl MapDelegate {
    /// What `mapView:viewForAnnotation:` answers.
    fn annotation_view(&self, annotation: &AnyObject) -> Option<Retained<NSView>> {
        let pin = annotation.downcast_ref::<MapPin>()?;
        let mtm = self.mtm();
        let none: Option<&NSString> = None;
        if pin.ivars().here {
            // SAFETY: MKAnnotationView's designated initialiser, with a live annotation and no reuse identifier.
            let dot: Retained<MKAnnotationView> =
                unsafe { msg_send![MKAnnotationView::alloc(mtm), initWithAnnotation: annotation, reuseIdentifier: none] };
            dot.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(DOT, DOT)));
            dot.setWantsLayer(true);
            if let Some(l) = dot.layer() {
                l.setCornerRadius(DOT / 2.0);
                l.setBackgroundColor(Some(&NSColor::systemBlueColor().CGColor()));
                l.setBorderColor(Some(&NSColor::whiteColor().CGColor()));
                l.setBorderWidth(DOT_RING);
            }
            return Some(dot.into_super());
        }
        if pin.ivars().href.is_empty() {
            return None;
        }
        // SAFETY: MKAnnotationView's designated initialiser, as above.
        let marker: Retained<MKMarkerAnnotationView> =
            unsafe { msg_send![MKMarkerAnnotationView::alloc(mtm), initWithAnnotation: annotation, reuseIdentifier: none] };
        let target: &AnyObject = self;
        // SAFETY: this delegate implements `openPin:`, taking the button; it outlives every map (Maps holds it).
        let open = unsafe { NSButton::buttonWithTitle_target_action(ns_string!("Open"), Some(target), Some(sel!(openPin:)), mtm) };
        // The page it opens, said under the pointer and read back by `openPin:`.
        open.setToolTip(Some(&NSString::from_str(&pin.ivars().href)));
        // SAFETY: MKAnnotationView's setters: a BOOL, and a view it retains.
        unsafe {
            let _: () = msg_send![&*marker, setCanShowCallout: true];
            let _: () = msg_send![&*marker, setRightCalloutAccessoryView: &*open];
        }
        Some(marker.into_super().into_super())
    }

    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars { open: RefCell::new(None) });
        // SAFETY: init is NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }

    /// Ask Apple Maps for the place `place` (an `MKMapItemIdentifier`'s string) for `want`. The answer comes on the
    /// main queue: a pin's goes to its card ([`Maps::swap`]), a point of interest's is its place card; one that fails
    /// is said once and leaves the map as it is.
    fn resolve(&self, place: &str, want: Want) {
        let (Some(id_class), Some(request_class)) = (AnyClass::get(c"MKMapItemIdentifier"), AnyClass::get(c"MKMapItemRequest")) else {
            eprintln!("harness-native: maps: place {place}: MapKit has no MKMapItemRequest (macOS 15's): its pin stays plain");
            return;
        };
        // SAFETY: -[MKMapItemIdentifier initWithIdentifierString:] with an NSString; nil for a string that is no
        // identifier.
        let id: Option<Retained<AnyObject>> = unsafe {
            let alloc: Allocated<AnyObject> = msg_send![id_class, alloc];
            msg_send![alloc, initWithIdentifierString: &*NSString::from_str(place)]
        };
        let Some(id) = id else {
            eprintln!("harness-native: maps: place {place}: not an Apple Maps item identifier (MKMapItemIdentifier refused it): its pin stays plain");
            return;
        };
        // SAFETY: MKMapItemRequest's designated initialiser with a live MKMapItemIdentifier.
        let request: Retained<AnyObject> = unsafe {
            let alloc: Allocated<AnyObject> = msg_send![request_class, alloc];
            msg_send![alloc, initWithMapItemIdentifier: &*id]
        };
        let token = NEXT.get();
        NEXT.set(token + 1);
        REQUESTS.with_borrow_mut(|r| r.push((token, request.clone())));
        let (delegate, place) = (self.retain(), place.to_owned());
        let done = RcBlock::new(move |item: *mut AnyObject, error: *mut AnyObject| {
            REQUESTS.with_borrow_mut(|r| r.retain(|(t, _)| *t != token));
            // SAFETY: a live MKMapItem, or null with an NSError (the handler's contract).
            let Some(item) = (unsafe { Retained::retain(item) }) else {
                // SAFETY: -[NSError code] is an NSInteger.
                let code: isize = if error.is_null() { -1 } else { unsafe { msg_send![&*error, code] } };
                eprintln!("harness-native: maps: place {place}: Apple Maps did not give it (MKError {code}: 4 is no such place, 2 a network failure, 3 its servers'): the map stays as it is");
                return;
            };
            match &want {
                Want::Pin { card } => {
                    if with(|st| st.maps.swap(*card, &place, &item)).is_none() {
                        eprintln!("harness-native: maps: place {place}: its answer came while the window was in use: its pin stays plain");
                    }
                }
                Want::Label { map, business } => delegate.label_resolved(map, *business, &item),
            }
        });
        // SAFETY: -getMapItemWithCompletionHandler: with a block of (MKMapItem*, NSError*), called once on the main
        // queue; the request is kept in REQUESTS until then.
        let _: () = unsafe { msg_send![&*request, getMapItemWithCompletionHandler: &*done] };
    }

    /// What `mapView:didSelectAnnotationView:` does: a place of Apple Maps shows its card, pointing at its pin.
    fn pin_selected(&self, map: &MKMapView, view: &MKAnnotationView) {
        // SAFETY: -[MKAnnotationView annotation] is an object or nil.
        let annotation: Option<Retained<AnyObject>> = unsafe { msg_send![view, annotation] };
        let Some(place) = annotation.and_then(|a| a.downcast::<MKMapItemAnnotation>().ok()) else { return };
        // SAFETY: -[MKMapItemAnnotation mapItem] is its MKMapItem, never nil.
        let item: Retained<AnyObject> = unsafe { msg_send![&*place, mapItem] };
        self.present(map, view, view.bounds(), &item, Shown::Pin(place));
    }

    /// What `mapView:didDeselectAnnotationView:` does: a pin's place card on `map` closes. Soon, not now: MapKit also
    /// lets a pin go while the window's state is in use (a map's pins replaced in [`Maps::sync`]).
    fn pin_deselected(&self, map: &MKMapView) {
        let pin = self.ivars().open.borrow().as_ref().is_some_and(|o| matches!(o.of, Shown::Pin(_)));
        if pin {
            self.close_soon_on(map);
        }
    }

    /// What `mapView:didSelectLabelMarker:` does: a label that is a place of Apple Maps is asked for; its card comes
    /// with the answer ([`MapDelegate::label_resolved`]).
    fn label_selected(&self, map: &MKMapView, marker: &AnyObject) {
        // SAFETY: a VKLabelMarker, whose `hasBusinessID` is a BOOL and `businessID` an unsigned 64-bit number
        // (`pois_pressable` was true for this map to hear of a label at all).
        let (has, business): (bool, u64) = unsafe {
            let has: bool = msg_send![marker, hasBusinessID];
            let business: u64 = msg_send![marker, businessID];
            (has, business)
        };
        if has {
            self.resolve(&format!("I{business:016X}"), Want::Label { map: map.retain(), business });
        }
    }

    /// The label of place `business` on `map`, while it is the one selected.
    fn selected_label(map: &MKMapView, business: u64) -> Option<Retained<AnyObject>> {
        // SAFETY: `_selectedLabelMarker` is a VKLabelMarker or nil, and its `businessID` an unsigned 64-bit number
        // ([`POI_SELECTORS`], [`LABEL_SELECTORS`]: only a map that answers them has a label selected).
        unsafe {
            let marker: Option<Retained<AnyObject>> = msg_send![map, _selectedLabelMarker];
            marker.filter(|m| {
                let its: u64 = msg_send![&**m, businessID];
                its == business
            })
        }
    }

    /// The place of a pressed point of interest came (`item`, an MKMapItem): its card, pointing at the label, unless
    /// the label was let go or its map left the window meanwhile.
    fn label_resolved(&self, map: &MKMapView, business: u64, item: &AnyObject) {
        if map.window().is_none() {
            return;
        }
        let Some(marker) = Self::selected_label(map, business) else { return };
        // SAFETY: -[VKLabelMarker coordinate] by value (`LabelAt`), and MKMapView's convertCoordinate:toPointToView:
        // with a CLLocationCoordinate2D by value and a view, answering a point in that view.
        let at: NSPoint = unsafe {
            let at: LabelAt = msg_send![&*marker, coordinate];
            let point: NSPoint = msg_send![map, convertCoordinate: Coordinate { latitude: at.latitude, longitude: at.longitude }, toPointToView: map];
            point
        };
        let rect = NSRect::new(NSPoint::new(at.x - LABEL_BOX / 2.0, at.y - LABEL_BOX / 2.0), NSSize::new(LABEL_BOX, LABEL_BOX));
        self.present(map, map, rect, item, Shown::Label(business));
    }

    /// Show Apple's place card of `item` (an MKMapItem) in a popover pointing at `rect` of `anchor` (a view in
    /// `map`), in the map's appearance, at once; the card that was up goes. Never called while the window's state is
    /// in use: a popover takes the keys, which the window hears of.
    fn present(&self, map: &MKMapView, anchor: &NSView, rect: NSRect, item: &AnyObject, of: Shown) {
        self.dismiss();
        let mtm = self.mtm();
        let Some(class) = AnyClass::get(c"MKMapItemDetailViewController") else {
            eprintln!("harness-native: maps: MapKit has no MKMapItemDetailViewController (macOS 15's): no place card is shown");
            return;
        };
        // SAFETY: -[MKMapItemDetailViewController initWithMapItem:displaysMap:] with a live MKMapItem and a BOOL (no
        // map of its own in the card), and its delegate (held weakly; Maps keeps this one alive), which implements
        // `mapItemDetailViewControllerDidFinish:`.
        let card: Retained<AnyObject> = unsafe {
            let alloc: Allocated<AnyObject> = msg_send![class, alloc];
            let card: Retained<AnyObject> = msg_send![alloc, initWithMapItem: item, displaysMap: false];
            let _: () = msg_send![&*card, setDelegate: self];
            card
        };
        let Ok(card) = card.downcast::<NSViewController>() else {
            eprintln!("harness-native: maps: MKMapItemDetailViewController is not an NSViewController here: no place card is shown");
            return;
        };
        let popover = NSPopover::new(mtm);
        popover.setContentViewController(Some(&*card));
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setAnimates(false);
        popover.setContentSize(NSSize::new(PLACE_CARD.0, PLACE_CARD.1));
        let look = anchor.effectiveAppearance();
        popover.setAppearance(Some(&*look));
        // SAFETY: NSPopover's delegate (held weakly; Maps keeps this one alive), which implements `popoverDidClose:`.
        let _: () = unsafe { msg_send![&*popover, setDelegate: self] };
        popover.showRelativeToRect_ofView_preferredEdge(rect, anchor, NSRectEdge::MaxX);
        if !popover.isShown() {
            eprintln!("harness-native: maps: a place card was not shown: what it points at is out of sight");
            return;
        }
        *self.ivars().open.borrow_mut() = Some(Open { map: map.retain(), popover, of });
    }

    /// Let go on the map of what the closed card `open` was of: its pin, or its label if that is still the one
    /// selected (a press on another label has already let it go).
    fn unselect(open: &Open) {
        // SAFETY: MKMapView's deselectAnnotation:animated: with an annotation and a BOOL (nothing happens for one not
        // selected), and `_deselectLabelMarkerAnimated:` with a BOOL ([`POI_SELECTORS`]).
        unsafe {
            match &open.of {
                Shown::Pin(pin) => {
                    let _: () = msg_send![&*open.map, deselectAnnotation: &**pin, animated: false];
                }
                Shown::Label(business) => {
                    if Self::selected_label(&open.map, *business).is_some() {
                        let _: () = msg_send![&*open.map, _deselectLabelMarkerAnimated: false];
                    }
                }
            }
        }
    }

    /// Close the place card, if one is up, and let go of what it was of. Never called while the window's state is in
    /// use (see [`MapDelegate::present`]).
    fn dismiss(&self) {
        // Taken first: closing calls `popoverDidClose:` and letting go `mapView:didDeselectAnnotationView:`, both of
        // which then find no card.
        let Some(open) = self.ivars().open.take() else { return };
        open.popover.close();
        Self::unselect(&open);
    }

    /// What `popoverDidClose:` does: the card that closed by itself (a press outside it) lets go of what it was of.
    fn popover_gone(&self, note: &NSNotification) {
        let Some(gone) = note.object() else { return };
        let ours = self.ivars().open.borrow().as_ref().is_some_and(|o| std::ptr::eq(Retained::as_ptr(&gone), Retained::as_ptr(&o.popover).cast()));
        if ours && let Some(open) = self.ivars().open.take() {
            Self::unselect(&open);
        }
    }

    /// Close the place card `popover`, if it is still the one up.
    fn close_if(&self, popover: &NSPopover) {
        let ours = self.ivars().open.borrow().as_ref().is_some_and(|o| std::ptr::eq(&*o.popover, popover));
        if ours {
            self.dismiss();
        }
    }

    /// Have the place card over `map`, if there is one, close on the run loop's next turn: for callers inside the
    /// window's state (a frame), where closing a popover now would hand the keys back unheard.
    fn close_soon_on(&self, map: &MKMapView) {
        let open = self.ivars().open.borrow();
        let Some(open) = open.as_ref().filter(|o| std::ptr::eq(&*o.map, map)) else { return };
        // SAFETY: NSObject's performSelector:withObject:afterDelay: (a selector, an object it retains until then, a
        // double); this delegate implements `closePlaceCard:`, taking the popover.
        let _: () = unsafe { msg_send![self, performSelector: sel!(closePlaceCard:), withObject: &*open.popover, afterDelay: 0.0_f64] };
    }
}

/// One card's views: the card (`id`, its node), `cut` at the part of it to be seen, `plate` at its box with its
/// corners, the map in it, the pins on it and the places of Apple Maps that took a pin's place (`items`); and what
/// they were last given: the places (`spec`), the two frames (in the content view's points), the corner radius (window
/// points) and the appearance.
struct Card {
    id: u64,
    cut: Retained<NSView>,
    plate: Retained<NSView>,
    map: Retained<MKMapView>,
    pins: Vec<Retained<MapPin>>,
    items: Vec<Retained<MKMapItemAnnotation>>,
    spec: Option<String>,
    frames: Option<(NSRect, NSRect)>,
    radius: Option<f64>,
    dark: Option<bool>,
}

/// The maps over the frame's Map cards.
pub struct Maps {
    main: Retained<HarnessView>,
    delegate: Retained<MapDelegate>,
    cards: Vec<Card>,
}

impl Maps {
    pub fn new(main: Retained<HarnessView>) -> Self {
        let delegate = MapDelegate::new(main.mtm());
        Self { main, delegate, cards: Vec::new() } // alloc: the few maps on screen, kept while their cards are
    }

    /// The place `place` of Apple Maps came (`item`, an MKMapItem) for a pin of card `card`: the pin that named it
    /// is replaced by the place's own annotation, the map left looking where it was. Nothing happens when the card or
    /// the pin is gone (its places changed meanwhile).
    fn swap(&mut self, card: u64, place: &str, item: &AnyObject) {
        let Some(card) = self.cards.iter_mut().find(|c| c.id == card) else { return };
        let Some(at) = card.pins.iter().position(|p| p.ivars().place == place) else { return };
        // SAFETY: MKMapItemAnnotation's designated initialiser with a live MKMapItem; nil when it has no place for it.
        let annotation: Option<Retained<MKMapItemAnnotation>> = unsafe { msg_send![MKMapItemAnnotation::alloc(), initWithMapItem: item] };
        let Some(annotation) = annotation else {
            eprintln!("harness-native: maps: card {}: place {place}: MapKit made no annotation of it: its pin stays plain", card.id);
            return;
        };
        let pin = card.pins.remove(at);
        // SAFETY: MKMapView's removeAnnotation: and addAnnotation: with live annotations.
        unsafe {
            let _: () = msg_send![&*card.map, removeAnnotation: &*pin];
            let _: () = msg_send![&*card.map, addAnnotation: &*annotation];
        }
        card.items.push(annotation);
    }

    /// This frame's cards (main view points at zoom `zoom`) under their maps: each placed, cut, rounded and in its
    /// appearance, its places set when they changed; the maps of cards no longer painted removed. A place card over
    /// a map that moved, changed its places or went closes.
    pub fn sync(&mut self, window: &NSWindow, cards: &[MapCard], zoom: f32) {
        let delegate = &self.delegate;
        self.cards.retain(|c| {
            let keep = cards.iter().any(|m| m.id == c.id);
            if !keep {
                delegate.close_soon_on(&c.map);
                c.cut.removeFromSuperview();
            }
            keep
        });
        if cards.is_empty() {
            return;
        }
        // SAFETY: the main view is in the window's content view for as long as the window lives (window.rs `build`).
        let Some(host) = (unsafe { self.main.superview() }) else {
            eprintln!("harness-native: maps: the main view has no superview to hold a map: {} Map cards are left empty", cards.len());
            return;
        };
        let host: &NSView = &host;
        let z = f64::from(zoom);
        for m in cards {
            let at = match self.cards.iter().position(|c| c.id == m.id) {
                Some(i) => i,
                None => {
                    let card = self.make(host, m.id);
                    self.cards.push(card);
                    self.cards.len() - 1
                }
            };
            let card = &mut self.cards[at];
            // The card's box and the part of it to be seen, in the content view's points (window points: view points at
            // the zoom, from the main view's flipped ones).
            let local = |r: [f32; 4]| {
                NSRect::new(NSPoint::new(f64::from(r[0]) * z, f64::from(r[1]) * z), NSSize::new(f64::from(r[2]) * z, f64::from(r[3]) * z))
            };
            let seen = self.main.convertRect_toView(local(m.seen), Some(host));
            let full = self.main.convertRect_toView(local(m.rect), Some(host));
            let shown = seen.size.width >= 1.0 && seen.size.height >= 1.0 && full.size.width >= 1.0 && full.size.height >= 1.0;
            let frames = shown.then_some((seen, full));
            if card.frames != frames {
                self.delegate.close_soon_on(&card.map);
                card.cut.setHidden(!shown);
                if shown {
                    card.cut.setFrame(seen);
                    // The plate where the card is, from the cut's own corner: the map stays still against its card.
                    card.plate.setFrame(NSRect::new(NSPoint::new(full.origin.x - seen.origin.x, full.origin.y - seen.origin.y), full.size));
                    card.map.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), full.size));
                }
                card.frames = frames;
            }
            let radius = f64::from(m.radius) * z;
            if card.radius != Some(radius) {
                if let Some(l) = card.plate.layer() {
                    l.setCornerRadius(radius);
                }
                card.radius = Some(radius);
            }
            if card.dark != Some(m.dark) {
                // SAFETY: the appearance names are valid static strings.
                let name = unsafe { if m.dark { NSAppearanceNameDarkAqua } else { NSAppearanceNameAqua } };
                card.cut.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
                card.dark = Some(m.dark);
            }
            // Its places, once it has a size to look at them in.
            if shown && card.spec.as_deref() != Some(m.spec.as_str()) {
                match serde_json::from_str::<serde_json::Value>(&m.spec).map_err(|e| e.to_string()).and_then(|v| Places::parse(&v)) {
                    Ok(places) => {
                        self.delegate.close_soon_on(&card.map);
                        show(card, &places, &self.delegate);
                    }
                    Err(e) => eprintln!("harness-native: maps: card {}: its places are not read ({e}): the map shows none", m.id),
                }
                card.spec = Some(m.spec.clone());
            }
        }
        // The keys stay the app's: should a view inside a map have taken them, the main view has them back.
        if let Some(first) = window.firstResponder()
            && let Ok(view) = first.downcast::<NSView>()
            && self.cards.iter().any(|c| view.isDescendantOf(&c.cut))
        {
            let main: &NSResponder = &self.main;
            window.makeFirstResponder(Some(main));
        }
    }

    /// The views of a card new to the frame, in `host` above the main view: nothing placed or shown yet.
    fn make(&self, host: &NSView, id: u64) -> Card {
        let mtm = self.main.mtm();
        let clipping = || {
            let v = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
            v.setWantsLayer(true);
            v.setClipsToBounds(true);
            if let Some(l) = v.layer() {
                l.setMasksToBounds(true);
            }
            v
        };
        let (cut, plate) = (clipping(), clipping());
        // SAFETY: initWithFrame: is NSView's designated initialiser, which MKMapView takes.
        let map: Retained<MKMapView> = unsafe { msg_send![MKMapView::alloc(mtm), initWithFrame: NSRect::ZERO] };
        // SAFETY: MKMapView's setters: its delegate (held weakly; Maps keeps it alive) and two BOOLs. A flat map, north
        // up: it pans and zooms, as the card's shape reads.
        unsafe {
            let _: () = msg_send![&*map, setDelegate: &*self.delegate];
            let _: () = msg_send![&*map, setPitchEnabled: false];
            let _: () = msg_send![&*map, setRotateEnabled: false];
        }
        // Apple's own points of interest are pressable, for their place cards.
        if pois_pressable() {
            // SAFETY: `_setCanSelectPOIs:` takes a BOOL ([`POI_SELECTORS`]; `pois_pressable` saw it answered).
            let _: () = unsafe { msg_send![&*map, _setCanSelectPOIs: true] };
        } else {
            eprintln!("harness-native: maps: card {id}: this MapKit lacks the private calls that select a point of interest (maps.rs POI_SELECTORS, LABEL_SELECTORS): only the card's own places show a place card");
        }
        plate.addSubview(&map);
        cut.addSubview(&plate);
        cut.setHidden(true);
        host.addSubview(&cut);
        Card { id, cut, plate, map, pins: Vec::new(), items: Vec::new(), spec: None, frames: None, radius: None, dark: None } // alloc: a map's pins, set with its places
    }
}

/// Set `card`'s map to `places`: its pins replaced, and looking where they say, at once (nothing animated). A place
/// that names its place of Apple Maps is asked for through `delegate`, and is the plain pin until the answer
/// ([`Maps::swap`]).
fn show(card: &mut Card, places: &Places, delegate: &MapDelegate) {
    // SAFETY: MKMapView's removeAnnotation: and addAnnotation: with our own live annotations, and setRegion:animated:
    // with an MKCoordinateRegion by value.
    unsafe {
        for pin in card.pins.drain(..) {
            let _: () = msg_send![&*card.map, removeAnnotation: &*pin];
        }
        for item in card.items.drain(..) {
            let _: () = msg_send![&*card.map, removeAnnotation: &*item];
        }
        if let Some(here) = places.here {
            card.pins.push(MapPin::new(here, "", "", true, "", ""));
        }
        for p in &places.pins {
            // A place with only a detail shows it as its title.
            let (title, subtitle) = if p.label.is_empty() { (p.detail.as_str(), "") } else { (p.label.as_str(), p.detail.as_str()) };
            card.pins.push(MapPin::new([p.lat, p.lon], title, subtitle, false, &p.href, &p.place));
        }
        for pin in &card.pins {
            let _: () = msg_send![&*card.map, addAnnotation: &**pin];
        }
        let size = card.map.bounds().size;
        let (center, span) = places.region(size.width, size.height);
        let region = Region {
            center: Coordinate { latitude: center[0], longitude: center[1] },
            span: Span { latitude_delta: span[0], longitude_delta: span[1] },
        };
        let _: () = msg_send![&*card.map, setRegion: region, animated: false];
    }
    for p in places.pins.iter().filter(|p| !p.place.is_empty()) {
        delegate.resolve(&p.place, Want::Pin { card: card.id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The private calls a pressable point of interest stands on exist in this system's MapKit and VectorKit
    /// ([`POI_SELECTORS`], [`LABEL_SELECTORS`]), and the public classes of a place card (macOS 15's) with them.
    #[test]
    fn pois_are_pressable() {
        assert!(answers(c"MKMapView", &POI_SELECTORS), "MKMapView lacks one of {POI_SELECTORS:?}");
        assert!(answers(c"VKLabelMarker", &LABEL_SELECTORS), "VKLabelMarker lacks one of {LABEL_SELECTORS:?}");
        assert!(super::pois_pressable());
        assert!(answers(c"MKMapItemIdentifier", &[c"initWithIdentifierString:"]));
        assert!(answers(c"MKMapItemRequest", &[c"initWithMapItemIdentifier:", c"getMapItemWithCompletionHandler:"]));
        assert!(answers(c"MKMapItemAnnotation", &[c"initWithMapItem:", c"mapItem"]));
        assert!(answers(c"MKMapItemDetailViewController", &[c"initWithMapItem:displaysMap:", c"setDelegate:"]));
    }
}
