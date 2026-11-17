//! The `Map` element's maps: Apple's map, live, over a cell's card. The app draws only the card (`hn.GenMap`, a quiet
//! rounded plate with the element's places on it as [`MapPlaces`], set by chat/genui/map.rs); the host covers the card
//! with a MapKit view of its own (window/maps.rs on macOS), which pans and zooms under the person's hand. What the host
//! needs each frame is here: [`App::emit`](super::App) notes every card the draw pass painted in the main window, where it
//! is and how much of it its clips leave to see (a transcript's scroll, a pane), as a [`MapCard`], and the host takes them
//! ([`App::take_maps`]). A card no longer painted (its row culled, its chat switched away, scrolled out) is not in the
//! list, so its view goes. Where no host shows them (a shot, harness-video, the phone) the card is its empty plate.
//!
//! What a map shows is [`Places`], read from the element's own props as the kernel sends them (mcp/src/ui/map.ts
//! `mapWire`): the pins, the person's place, and where it looks ([`Places::region`]).
use super::*;
use serde_json::Value;

/// A web map's zoom a lone place (or `here` alone) is shown at, and the deepest a fit to the places goes: a few streets.
const NEAR_ZOOM: f64 = 14.0;
/// The zoom `center` is shown at when none is said: a town.
const CENTER_ZOOM: f64 = 13.0;
/// How much wider and taller than the places' own box a fit looks, so a pin's balloon and its words are inside it.
const FIT_ROOM: f64 = 1.4;

/// On an `hn.GenMap` card: the places it shows, the element's own props (`markers`, `here`, `center`, `zoom`) as JSON.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MapPlaces {
    pub spec: ScriptString,
}

pub(super) fn register(world: &World) {
    world.component::<MapPlaces>();
}

/// One map card the main window painted this frame: its node, its box and the part of it to be seen (`[x, y, w, h]`,
/// view points; `seen` is the box cut by every clip around it), its corner radius (view points), whether it is drawn in
/// the dark appearance, and its places ([`MapPlaces::spec`]).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub struct MapCard {
    pub id: u64,
    pub rect: [f32; 4],
    pub seen: [f32; 4],
    pub radius: f32,
    pub dark: bool,
    pub spec: String,
}

/// One place: where it is, its pin's title and the line under it, the page a press on its callout opens, and which
/// place of Apple Maps it is (`place`, an `MKMapItem`'s identifier such as "I8EE01663EF4733A0"; empty when unknown): a
/// pin that says so shows Apple's own place card when pressed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pin {
    pub lat: f64,
    pub lon: f64,
    pub label: String,
    pub detail: String,
    pub href: String,
    pub place: String,
}

/// What a map shows: its pins, the person's place, and where it looks (`center` and `zoom` as said, else fitted).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Places {
    pub pins: Vec<Pin>,
    pub here: Option<[f64; 2]>,
    pub center: Option<[f64; 2]>,
    pub zoom: Option<f64>,
}

/// `[lat, lon]` when both are numbers on the earth.
fn coords(lat: &Value, lon: &Value) -> Option<[f64; 2]> {
    let (lat, lon) = (lat.as_f64()?, lon.as_f64()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some([lat, lon])
}

/// `[lat, lon]` from `v`, or the problem with it as prop `what`.
fn point(v: &Value, what: &str) -> Result<[f64; 2], String> {
    v.as_array()
        .filter(|a| a.len() == 2)
        .and_then(|a| coords(&a[0], &a[1]))
        .ok_or_else(|| format!("<Map> {what} is {v}, not [lat, lon] (a latitude from -90 to 90, a longitude from -180 to 180)"))
}

/// The degrees of latitude and longitude a view `w` by `h` points shows at a web map's `zoom` (256-pixel tiles: the
/// world is `256 * 2^zoom` points wide) around latitude `lat`, as the span MapKit is asked for. The longitude is the
/// exact one; the latitude is half of what the view's height shows there, so MapKit, which shows at least the span
/// asked for on both axes, lets the width decide.
pub fn span_of(zoom: f64, w: f64, h: f64, lat: f64) -> [f64; 2] {
    let lon = (360.0 * w.max(1.0) / (256.0 * zoom.exp2())).min(360.0);
    let tall = lon * (h.max(1.0) / w.max(1.0)) * lat.to_radians().cos().abs();
    [(tall * 0.5).clamp(1e-6, 180.0), lon]
}

impl Places {
    /// The places `props` (a `Map` element's, as mcp/src/ui/map.ts `mapWire` sends them) say, or why they are none.
    pub fn parse(props: &Value) -> Result<Self, String> {
        let mut out = Self::default();
        match &props["markers"] {
            Value::Null => {}
            Value::Array(list) => {
                for (i, m) in list.iter().enumerate() {
                    let Some([lat, lon]) = coords(&m["lat"], &m["lon"]) else {
                        let said: String = m.to_string().chars().take(48).collect();
                        return Err(format!(
                            "<Map> markers[{i}] is {said}, not a place {{lat, lon, label?, detail?, href?, place?}} (a latitude from -90 to 90, a longitude from -180 to 180)"
                        ));
                    };
                    let words = |name: &str| m[name].as_str().unwrap_or("").to_owned();
                    out.pins.push(Pin { lat, lon, label: words("label"), detail: words("detail"), href: words("href"), place: words("place") });
                }
            }
            v => {
                let said: String = v.to_string().chars().take(48).collect();
                return Err(format!("<Map> markers is {said}, not a list of places: markers={{[{{lat, lon, label}}]}}"));
            }
        }
        for (name, into) in [("here", &mut out.here), ("center", &mut out.center)] {
            if !props[name].is_null() {
                *into = Some(point(&props[name], name)?);
            }
        }
        match &props["zoom"] {
            Value::Null => {}
            v => match v.as_f64().filter(|z| (0.0..=19.0).contains(z)) {
                Some(z) => out.zoom = Some(z),
                None => return Err(format!("<Map> zoom is {v}, not a number from 0 (the world) to 19 (a building)")),
            },
        }
        if out.pins.is_empty() && out.here.is_none() && out.center.is_none() {
            return Err("<Map> has nothing to show: markers={[{lat, lon, label}]}, center={[lat, lon]} or here={[lat, lon]}".into());
        }
        Ok(out)
    }

    /// Where a view `w` by `h` points looks: its centre `[lat, lon]` and the span `[latitude, longitude]` in degrees
    /// (MapKit's region). `center` with its zoom (a town when none is said); with no places, the person's place a few
    /// streets wide; else the places' own box, at the zoom said or fitted with room around it, never nearer than a few
    /// streets.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn region(&self, w: f64, h: f64) -> ([f64; 2], [f64; 2]) {
        if let Some(c) = self.center {
            return (c, span_of(self.zoom.unwrap_or(CENTER_ZOOM), w, h, c[0]));
        }
        if self.pins.is_empty() {
            let c = self.here.unwrap_or_default();
            return (c, span_of(self.zoom.unwrap_or(NEAR_ZOOM), w, h, c[0]));
        }
        let (mut south, mut north, mut west, mut east) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in &self.pins {
            (south, north) = (south.min(p.lat), north.max(p.lat));
            (west, east) = (west.min(p.lon), east.max(p.lon));
        }
        let c = [(south + north) / 2.0, (west + east) / 2.0];
        if let Some(z) = self.zoom {
            return (c, span_of(z, w, h, c[0]));
        }
        let near = span_of(NEAR_ZOOM, w, h, c[0]);
        (c, [((north - south) * FIT_ROOM).clamp(near[0], 180.0), ((east - west) * FIT_ROOM).clamp(near[1], 360.0)])
    }
}

impl App {
    /// The map cards the last frame painted in the main window, for the host to cover with its maps.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn take_maps(&mut self) -> Vec<MapCard> {
        std::mem::take(&mut self.maps)
    }
}

/// The card of a painted map node `e`, drawn at `rect` (view points, its transition's scale applied) as the draw pass
/// said of it (`p`) in a window whose appearance is `dark`; None when it carries no places.
pub(super) fn card(e: EntityView<'_>, p: &harness_native_ui_draw_api::Painted, rect: [f32; 4], dark: bool) -> Option<MapCard> {
    let places = e.try_cloned::<&MapPlaces>()?;
    let seen = if p.clipped {
        let (x0, y0) = (rect[0].max(p.clip_x), rect[1].max(p.clip_y));
        let (x1, y1) = ((rect[0] + rect[2]).min(p.clip_x + p.clip_w), (rect[1] + rect[3]).min(p.clip_y + p.clip_h));
        [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
    } else {
        rect
    };
    let radius = e.try_cloned::<&Shape>().map_or(0.0, |s| s.radius) * p.scale;
    Some(MapCard { id: *e.id(), rect, seen, radius, dark: p.dark(dark), spec: places.spec.as_str().to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The zoom is a web map's: at 13, a view 800 points wide shows 360 * 800 / (256 * 2^13) degrees of longitude
    /// (MapKit, asked for this span in an 800 by 480 view, shows exactly that longitude: measured on macOS 27).
    #[test]
    fn a_zoom_is_a_web_maps() {
        let [lat, lon] = span_of(13.0, 800.0, 480.0, 34.1);
        assert!((lon - 0.137_329).abs() < 1e-6, "{lon}");
        assert!(lat > 0.0 && lat < lon * 0.6, "{lat}");
        let world = span_of(0.0, 800.0, 480.0, 0.0);
        assert_eq!(world[1], 360.0);
    }

    /// One place alone is shown a few streets wide, never at the deepest zoom; places apart are all inside the span.
    #[test]
    fn a_fit_holds_every_place() {
        let one = Places::parse(&serde_json::json!({"markers": [{"lat": 34.1, "lon": -118.1, "label": "A", "place": "I8EE01663EF4733A0"}]})).unwrap();
        assert_eq!((one.pins[0].place.as_str(), one.pins[0].href.as_str()), ("I8EE01663EF4733A0", ""));
        assert_eq!(one.region(800.0, 480.0), ([34.1, -118.1], span_of(NEAR_ZOOM, 800.0, 480.0, 34.1)));
        let two = Places::parse(&serde_json::json!({"markers": [{"lat": 34.0, "lon": -118.0}, {"lat": 36.0, "lon": -122.0}], "here": [35.0, -120.0]})).unwrap();
        let (c, span) = two.region(800.0, 480.0);
        assert_eq!(c, [35.0, -120.0]);
        assert!(span[0] > 2.0 && span[1] > 4.0, "{span:?}");
        assert!(Places::parse(&serde_json::json!({"markers": []})).is_err());
        assert!(Places::parse(&serde_json::json!({"center": [91.0, 0.0]})).is_err());
    }
}
