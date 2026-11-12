import { expect, test } from "bun:test";
import { ASSUMED_MS, LONG_MS, NOTICES, gate, limit, noticeAt, span, stopped, told } from "./patience.ts";

test("a duration reads as a person would say it", () => {
  expect(span(120)).toBe("0.12s");
  expect(span(30_000)).toBe("30.0s");
  expect(span(90_000)).toBe("1m30s");
  expect(span(47 * 60_000 + 2_000)).toBe("47m02s");
  expect(span(3 * 3600_000)).toBe("3h00m");
});

test("a long run is refused until it says why, and a quick one never is", () => {
  expect(gate(undefined, undefined)).toBeNull();
  expect(gate(ASSUMED_MS, undefined)).toBeNull();
  expect(gate(LONG_MS, undefined)).toBeNull();
  const refused = gate(LONG_MS + 1, undefined);
  expect(refused).toContain("Not run");
  expect(refused).toContain("why");
  expect(gate(20 * 60_000, "tests")).not.toBeNull();
  expect(gate(20 * 60_000, "the cold build is the thing being measured; no cache can stand in for it")).toBeNull();
  expect(gate(0, undefined)).toContain("expect_ms");
  expect(gate(Number.NaN, undefined)).toContain("expect_ms");
});

test("a cell's limit is the caller's, else follows the estimate, else there is none", () => {
  expect(limit(undefined, undefined)).toBeUndefined();
  expect(limit(undefined, 5_000)).toBe(5_000);
  expect(limit(10_000, 5_000)).toBe(5_000);
  expect(limit(10_000, undefined)).toBe(70_000);
  expect(limit(60_000, undefined)).toBe(240_000);
});

test("the reply says the time against the estimate, and advises only when it is over", () => {
  const on = told({ elapsed_ms: 12_000, expect_ms: 10_000, running: false, session_ms: 0 });
  expect(on.time).toBe("12.0s · expected 10.0s");
  expect(on.advice).toBeNull();
  const over = told({ elapsed_ms: 47 * 60_000, expect_ms: 5 * 60_000, running: false, session_ms: 0 });
  expect(over.time).toBe("47m00s · expected 5m00s · 9.4× over");
  expect(over.advice).toContain("find out why");
  const still = told({ elapsed_ms: 60_000, expect_ms: 10_000, running: true, session_ms: 0 });
  expect(still.advice).toContain("still running");
});

test("a long cell with no estimate is told it should have had one; a quick one is left alone", () => {
  expect(told({ elapsed_ms: 800, running: false, session_ms: 0 })).toEqual({ time: "0.80s", advice: null });
  const long = told({ elapsed_ms: 14 * 60_000, running: false, session_ms: 0 });
  expect(long.time).toBe("14m00s · no estimate given");
  expect(long.advice).toContain("expect_ms");
});

test("the session's waiting is said once it is worth saying", () => {
  expect(told({ elapsed_ms: 1_000, running: false, session_ms: 9 * 60_000 }).time).toBe("1.00s");
  expect(told({ elapsed_ms: 1_000, running: false, session_ms: 3 * 3600_000 + 12 * 60_000 }).time).toBe("1.00s · 3h12m waited in cells this session");
});

test("a stopped cell is told it was suspiciously slow, not just that it timed out", () => {
  expect(stopped(40_000, 10_000)).toContain("suspiciously slow");
  expect(stopped(5_000, undefined)).toBe("cell stopped at its limit of 5.00s");
});

test("a backgrounded cell speaks up at 2×, 4×, 8× and 16× its estimate, then stays quiet", () => {
  expect([0, 1, 2, 3].map((n) => noticeAt(10_000, n))).toEqual([20_000, 40_000, 80_000, 160_000]);
  expect(noticeAt(10_000, NOTICES)).toBeUndefined();
  expect([0, 1].map((n) => noticeAt(undefined, n))).toEqual([120_000, 480_000]);
});
