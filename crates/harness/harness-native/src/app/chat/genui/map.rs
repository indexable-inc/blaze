//! The `Map` element: places on Apple's map, `<Map markers={[{lat, lon, label, detail, href}]} here={…} />`, drawn as
//! hn.GenMap. Only the card's places are set here (`MapPlaces`, the element's own props as the kernel's map.ts `mapWire`
//! sends them); the map itself is the host's, a MapKit view laid over the card every frame (src/app/maps.rs says which
//! cards are on screen, src/window/maps.rs covers them).
use super::*;
use crate::app::maps::{MapPlaces, Places};

/// The element's own props, apart from the ones every element takes.
fn own(node: &Value) -> Value {
    let mut own = serde_json::Map::new();
    for name in ["markers", "here", "center", "zoom"] {
        if !prop(node, name).is_null() {
            own.insert(name.to_owned(), prop(node, name).clone());
        }
    }
    Value::Object(own)
}

/// Why a `Map` element is not drawn: places that are none (the kernel checks them first; a tree from elsewhere may not be).
pub(super) fn check(node: &Value) -> Option<String> {
    Places::parse(&own(node)).err()
}

/// Set the card's places: the same ones leave the host's map as the person moved it, new ones have it look again.
pub(super) fn element(v: View<'_>, node: &Value) {
    let want = MapPlaces { spec: own(node).to_string().as_str().into() };
    if v.entity().try_cloned::<&MapPlaces>().as_ref() != Some(&want) {
        v.set(want);
    }
}
