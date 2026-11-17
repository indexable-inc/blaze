import { expect, test } from "bun:test";
import { distance, hint, missingName, nearest } from "./help.ts";

test("the nearest name: a typo, a case, a part", () => {
  expect(distance("glob", "globb")).toBe(1);
  expect(nearest("globb", ["glob", "rg", "ls"])).toEqual(["glob"]);
  expect(nearest("gitlog", ["gitLog", "glob"])).toEqual(["gitLog"]);
  expect(nearest("search", ["googleSearch", "rg"])).toEqual(["googleSearch"]);
  expect(nearest("zzz", ["glob", "rg"])).toEqual([]);
});

test("an error about a name that is not there says the nearest one and where to look", () => {
  expect(missingName("globb is not defined")).toBe("globb");
  expect(missingName("cargo.tset is not a function. (In 'cargo.tset()', 'cargo.tset' is undefined)")).toBe("cargo.tset");
  expect(missingName("Cannot find name 'tabel'. Did you mean 'table'?")).toBe("tabel");
  expect(missingName("x is null")).toBeUndefined();
  const names = ["glob", "cargo", "cargo.test", "table"];
  expect(hint("globb is not defined", names)).toBe("No `globb` here. Nearest: glob (help('glob')).");
  expect(hint("cargo.tset is not a function", names)).toContain("cargo.test (help('cargo.test'))");
  expect(hint("frobnicate is not defined", names)).toContain("help.search(");
  expect(hint("division by zero", names)).toBeNull();
});
