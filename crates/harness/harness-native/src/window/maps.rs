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
//! Appearance: each card's views take the appearance its card is drawn in (the window's, or its `Scheme`'s), so the map
//! is Apple's dark or light one with the app.
//!
//! Keys stay the app's: `MKMapView` refuses first responder on macOS, and should anything inside a map take it, the
//! next frame hands it back to the main view (the composer keeps the typing).
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
use objc2::encode::{Encode, Encoding};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, extern_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSButton, NSColor, NSResponder, NSView, NSWindow,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, ns_string};

#[link(name = "MapKit", kind = "framework")]
unsafe extern "C" {}

/// The blue dot's width (points) and its white ring's.
const DOT: f64 = 16.0;
const DOT_RING: f64 = 3.0;

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

/// What a pin of ours is beyond its point and words: the person's place (drawn as the dot), or a place with the page
/// its callout opens (empty: none).
struct PinIvars {
    here: bool,
    href: String,
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
    fn new(at: [f64; 2], title: &str, subtitle: &str, here: bool, href: &str) -> Retained<Self> {
        let this = Self::alloc().set_ivars(PinIvars { here, href: href.to_owned() });
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

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HarnessMapDelegate"]
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
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: init is NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

/// One card's views: the card (`id`, its node), `cut` at the part of it to be seen, `plate` at its box with its
/// corners, the map in it and the pins on it; and what they were last given: the places (`spec`), the two frames (in
/// the content view's points), the corner radius (window points) and the appearance.
struct Card {
    id: u64,
    cut: Retained<NSView>,
    plate: Retained<NSView>,
    map: Retained<MKMapView>,
    pins: Vec<Retained<MapPin>>,
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

    /// This frame's cards (main view points at zoom `zoom`) under their maps: each placed, cut, rounded and in its
    /// appearance, its places set when they changed; the maps of cards no longer painted removed.
    pub fn sync(&mut self, window: &NSWindow, cards: &[MapCard], zoom: f32) {
        self.cards.retain(|c| {
            let keep = cards.iter().any(|m| m.id == c.id);
            if !keep {
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
                    Ok(places) => show(card, &places),
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
        plate.addSubview(&map);
        cut.addSubview(&plate);
        cut.setHidden(true);
        host.addSubview(&cut);
        Card { id, cut, plate, map, pins: Vec::new(), spec: None, frames: None, radius: None, dark: None } // alloc: a map's pins, set with its places
    }
}

/// Set `card`'s map to `places`: its pins replaced, and looking where they say, at once (nothing animated).
fn show(card: &mut Card, places: &Places) {
    // SAFETY: MKMapView's removeAnnotation: and addAnnotation: with our own live annotations, and setRegion:animated:
    // with an MKCoordinateRegion by value.
    unsafe {
        for pin in card.pins.drain(..) {
            let _: () = msg_send![&*card.map, removeAnnotation: &*pin];
        }
        if let Some(here) = places.here {
            card.pins.push(MapPin::new(here, "", "", true, ""));
        }
        for p in &places.pins {
            // A place with only a detail shows it as its title.
            let (title, subtitle) = if p.label.is_empty() { (p.detail.as_str(), "") } else { (p.label.as_str(), p.detail.as_str()) };
            card.pins.push(MapPin::new([p.lat, p.lon], title, subtitle, false, &p.href));
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
}
