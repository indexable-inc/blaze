import { expect, test } from "bun:test";
import { checkProp } from "./element.ts";
import { MAX_MARKERS, mapOf, mapWire, markersOf, pointOf } from "./map.ts";

test("a place is {lat, lon, label?, detail?, href?}, and each problem names the place and what it should be", () => {
  expect(markersOf([{ lat: 34.1, lon: -118.1, label: "Cha Redefine", detail: "0.1 mi", href: " https://example.com/a " }])).toEqual([
    { lat: 34.1, lon: -118.1, label: "Cha Redefine", detail: "0.1 mi", href: "https://example.com/a" },
  ]);
  expect(markersOf("x")).toContain("not a list");
  expect(markersOf([{ lat: 91, lon: 0 }])).toBe("markers[0] has lat 91: a latitude is a number from -90 to 90");
  expect(markersOf([{ lat: 0, lon: 0 }, { lat: 0, lon: 181 }])).toBe("markers[1] has lon 181: a longitude is a number from -180 to 180");
  expect(markersOf([{ lat: 0, lng: 0 }])).toContain(`markers[0] has "lng" (say lon)`);
  expect(markersOf([{ lat: "34", lon: 0 }])).toContain("markers[0] has lat \"34\"");
  expect(markersOf([{ lat: 0, lon: 0, href: "javascript:alert(1)" }])).toContain("an http(s) URL");
  expect(markersOf(Array.from({ length: MAX_MARKERS + 1 }, () => ({ lat: 0, lon: 0 })))).toContain(`at most ${MAX_MARKERS}`);
});

test("a point is [lat, lon], {lat, lon} or what location() returns", () => {
  expect(pointOf([34.1, -118.1], "here")).toEqual([34.1, -118.1]);
  expect(pointOf({ lat: 1, lon: 2 }, "center")).toEqual([1, 2]);
  expect(pointOf({ latitude: 1, longitude: 2, accuracy_m: 30 }, "here")).toEqual([1, 2]);
  expect(pointOf([1], "center")).toContain("center has 1 numbers");
  expect(pointOf([1, 200], "center")).toContain("center has lon 200");
  expect(pointOf("LA", "here")).toContain(`here is "LA"`);
});

test("a Map needs something to look at, and says so as the element's error", () => {
  expect(mapOf({})).toContain("has nothing to show");
  expect(mapOf({ markers: [] })).toContain("has nothing to show");
  expect(mapOf({ center: [1, 2], zoom: 25 })).toContain("zoom is 25");
  expect(mapOf({ markers: [{ lat: 1, lon: 2, label: "A" }], here: { latitude: 3, longitude: 4 } })).toEqual({
    markers: [{ lat: 1, lon: 2, label: "A" }], here: [3, 4],
  });
  expect(checkProp("Map", "markers", [{ lat: 95, lon: 0 }], "line 1")?.message).toContain("on <Map> is not a list of at most 200 places");
  expect(checkProp("Map", "pins", [], "line 1")?.message).toContain(`has no prop "pins"`);
});

test("the element on the wire is what the app's genui/map.rs reads: markers always, here, center and zoom only when said, points as [lat, lon]", () => {
  const wire = (props: Record<string, unknown>) => {
    const spec = mapOf(props);
    if (typeof spec === "string") throw new Error(spec);
    return mapWire(spec);
  };
  expect(wire({ markers: [{ lat: 1, lon: 2, label: "A", detail: "near", href: "https://example.com" }], here: { latitude: 3, longitude: 4, accuracy_m: 30 } })).toEqual({
    markers: [{ lat: 1, lon: 2, label: "A", detail: "near", href: "https://example.com" }], here: [3, 4],
  });
  expect(wire({ center: { lat: 5, lon: 6 }, zoom: "12" })).toEqual({ markers: [], center: [5, 6], zoom: 12 });
  expect(Object.keys(wire({ here: [1, 2] })).sort()).toEqual(["here", "markers"]);
});
