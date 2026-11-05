// Tables (src/table.ts): the polars verbs on the eager array Table, the old verb names, groupBy's two agg forms, join's
// hows, the lazy plan and its collect, column names checked by type and at run time, and gitLog on this repository.
import { expect, test } from "bun:test";
import { LazyTable, Table, col, lit, table } from "./table.ts";
import { gitLog } from "./git-log.ts";

type Pet = { name: string; kind: string; age: number; owner: { name: string } | null; tags: string[] };
const pets: Pet[] = [
  { name: "rex", kind: "dog", age: 4, owner: { name: "ana" }, tags: ["loud", "big"] },
  { name: "tom", kind: "cat", age: 7, owner: null, tags: [] },
  { name: "fido", kind: "dog", age: 2, owner: { name: "bo" }, tags: ["small"] },
  { name: "kit", kind: "cat", age: 1, owner: { name: "ana" }, tags: ["small"] },
];
const plain = <T>(rows: readonly T[]): T[] => [...rows];

test("a Table is still a real array, and Array's own slice, sort(fn) and join(sep) still work", () => {
  const t = table(pets);
  expect(Array.isArray(t)).toBe(true);
  expect(t.length).toBe(4);
  expect(t[0]!.name).toBe("rex");
  const names = t.map((p) => p.name);
  expect(names instanceof Table).toBe(false);
  expect(t.slice(1, 3).map((p) => p.name)).toEqual(["tom", "fido"]);
  expect(t.slice(1, 3) instanceof Table).toBe(false);
  expect(table([3, 1, 2].map((n) => ({ n }))).sort((a, b) => a.n - b.n).map((r) => r.n)).toEqual([1, 2, 3]);
  expect(table([{ a: 1 }]).join()).toBe("[object Object]");
  expect(names.join(",")).toBe("rex,tom,fido,kit");
  expect(table.from(pets) instanceof Table).toBe(true);
});

test("polars verbs on the eager Table, with polars names and argument order", () => {
  const t = table(pets);
  expect(plain(t.filter(col("kind").eq("dog")).select("name", "age").sort("age"))).toEqual([{ name: "fido", age: 2 }, { name: "rex", age: 4 }]);
  const f = t.filter((p) => p.age > 3);
  expect(f instanceof Table).toBe(true);
  expect(f.toArray("name")).toEqual(["rex", "tom"]);
  expect(t.sort("age", { descending: true }).head(2).toArray("name")).toEqual(["tom", "rex"]);
  expect(t.sort("age", true).tail(1).toArray("name")).toEqual(["kit"]);
  expect(plain(t.withColumns(col("age").mul(12).alias("months"), col("name").str.toUpperCase()).select("name", "months").head(1))).toEqual([{ name: "REX", months: 48 }]);
  expect(plain(t.select(col("age").max().alias("oldest"), col("age").mean().alias("mean")))).toEqual([{ oldest: 7, mean: 3.5 }]);
  expect(t.unique("kind").toArray("name")).toEqual(["rex", "tom"]);
  expect(t.unique("kind", { keep: "last" }).toArray("name")).toEqual(["fido", "kit"]);
  expect(t.unique(["kind"]).toArray("name")).toEqual(["rex", "tom"]);
  expect(plain(t.rename({ name: "pet" }).select("pet").head(1))).toEqual([{ pet: "rex" }]);
  expect(plain(t.count())).toEqual([{ name: 4, kind: 4, age: 4, owner: 3, tags: 4 }]);
  expect(t.filter(col("age").gtEq(2).and(col("kind").neq("cat"))).toArray("name")).toEqual(["rex", "fido"]);
  expect(t.filter(col("name").isIn(["tom", "kit"]).not()).toArray("name")).toEqual(["rex", "fido"]);
  expect(t.filter(col("owner").isNull()).toArray("name")).toEqual(["tom"]);
  expect(t.filter(col("tags").list.contains("small")).toArray("name")).toEqual(["fido", "kit"]);
  expect(t.select("name", col("owner").struct.field("name").alias("who")).filter(col("who").eq("ana")).toArray("name")).toEqual(["rex", "kit"]);
  expect(t.select(col("age").add(lit(1)).alias("next")).toArray("next")).toEqual([5, 8, 3, 2]);
  const exploded = t.explode("tags").select("name", "tags");
  expect(plain(exploded)).toEqual([{ name: "rex", tags: "loud" }, { name: "rex", tags: "big" }, { name: "tom", tags: null }, { name: "fido", tags: "small" }, { name: "kit", tags: "small" }]);
  expect(t.height()).toBe(4);
});

test("the old verb names still work", () => {
  const t = table(pets);
  expect(t.where((p) => p.kind === "cat").column("name")).toEqual(["tom", "kit"]);
  expect(t.derive({ months: (p) => p.age * 12 }).column("months")).toEqual([48, 84, 24, 12]);
  expect(t.sortBy("age", { desc: true }).column("name")).toEqual(["tom", "rex", "fido", "kit"]);
  expect(t.sortBy(["kind", "age"]).column("name")).toEqual(["kit", "tom", "fido", "rex"]);
  expect(Object.keys(t.drop("owner", "tags")[0]!)).toEqual(["name", "kind", "age"]);
  expect(t.unique("kind", "name").length).toBe(4);
  expect(plain(table([{ id: 1, meta: { a: 1, b: 2 } }]).unnest("meta"))).toEqual([{ id: 1, a: 1, b: 2 }]);
  const owners = [{ who: "ana", city: "Oslo" }];
  expect(plain(t.select("name", col("owner").struct.field("name").alias("who")).joinOn(owners, "who"))).toEqual([{ name: "rex", who: "ana", city: "Oslo" }, { name: "kit", who: "ana", city: "Oslo" }]);
  expect(t.select("name", col("owner").struct.field("name").alias("who")).joinOn(owners, "who", "left").length).toBe(4);
  expect(t.rows() instanceof Table).toBe(false);
});

test("groupBy(...).agg takes polars expressions or the old callback, and .len()", () => {
  const t = table(pets);
  expect(plain(t.groupBy("kind").agg(col("age").sum().alias("years"), col("name").count().alias("n"), col("name").implode().alias("names"))))
    .toEqual([{ kind: "dog", years: 6, n: 2, names: ["rex", "fido"] }, { kind: "cat", years: 8, n: 2, names: ["tom", "kit"] }]);
  const old = t.groupBy("kind").agg((a) => ({ n: a.count(), years: a.sum("age"), oldest: a.max("age"), names: a.list("name") }));
  expect(plain(old)).toEqual([{ kind: "dog", n: 2, years: 6, oldest: 4, names: ["rex", "fido"] }, { kind: "cat", n: 2, years: 8, oldest: 7, names: ["tom", "kit"] }]);
  const n: number = old[0]!.years;
  expect(n).toBe(6);
  expect(plain(t.groupBy("kind").len().sort("kind"))).toEqual([{ kind: "cat", len: 2 }, { kind: "dog", len: 2 }]);
  expect(t.groupBy("kind").agg((a) => ({ sub: a.rows() }))[0]!.sub instanceof Table).toBe(true);
});

test("joins are inner, left, semi or anti, and a clashing column gets the suffix", () => {
  const owners = table([{ name: "ana", city: "Oslo" }, { name: "bo", city: "Rome" }]);
  const by = table(pets).select(col("name").alias("pet"), col("owner").struct.field("name").alias("name"));
  expect(plain(by.join(owners, { on: "name" }))).toEqual([{ pet: "rex", name: "ana", city: "Oslo" }, { pet: "fido", name: "bo", city: "Rome" }, { pet: "kit", name: "ana", city: "Oslo" }]);
  expect(by.join(owners, { on: "name", how: "left" }).filter(col("city").isNull()).toArray("pet")).toEqual(["tom"]);
  expect(by.join(owners, { on: "name", how: "semi" }).length).toBe(3);
  expect(by.join(owners, { on: "name", how: "anti" }).toArray("pet")).toEqual(["tom"]);
  expect(plain(table([{ id: 1, v: "a" }]).join([{ id: 1, v: "b" }], { on: "id" }))).toEqual([{ id: 1, v: "a", v_right: "b" }]);
  expect(plain(table([{ id: 1, v: "a" }]).join([{ id: 1, v: "b" }], { on: "id", suffix: "_2" }))).toEqual([{ id: 1, v: "a", v_2: "b" }]);
});

test("a LazyTable records a plan, runs on await or collect, and resolves to the eager Table", async () => {
  let scans = 0;
  const lazy = table.lazy(async () => { scans++; return pets; }).filter(col("age").gt(1)).groupBy("kind").agg(col("age").sum().alias("years")).sort("years", true);
  expect(lazy instanceof LazyTable).toBe(true);
  expect(scans).toBe(0);
  expect(lazy.explain()).toBe('rows(<function>)\nfilter(col("age").gt(1))\ngroupBy(kind).agg(col("age").sum().alias("years"))\nsort(years desc)');
  const done = await lazy;
  expect(done instanceof Table).toBe(true);
  expect(plain(done)).toEqual([{ kind: "cat", years: 7 }, { kind: "dog", years: 6 }]);
  expect(plain(await lazy.collect())).toEqual(plain(done));
  expect(await lazy.toArray("kind")).toEqual(["cat", "dog"]);
  expect(await lazy.height()).toBe(2);
  expect(scans).toBe(4);
  // A promise source chains before any await, and is awaited once.
  const fromPromise = table.lazy(Promise.resolve(pets)).explode("tags").filter(col("tags").isNotNull()).groupBy("tags").len().sort("len", true).head(1);
  expect(await fromPromise.toRecords()).toEqual([{ tags: "small", len: 2 }]);
  // polars' slice is the LazyTable's; a returned thenable resolves like the cell body's `return await`.
  expect(await table.lazy(pets).slice(1, 2).toArray("name")).toEqual(["tom", "fido"]);
  expect(await table.lazy(pets).slice(-1).toArray("name")).toEqual(["kit"]);
  expect((await (async () => table.lazy(pets).head(1))())[0]!.name).toBe("rex");
  expect(await table(pets).lazy().join(table.lazy([{ name: "rex", vet: "x" }]), { on: "name", how: "semi" }).toArray("name")).toEqual(["rex"]);
  expect(plain(await table.lazy(pets).groupBy("kind").agg((a) => ({ n: a.count() })))).toEqual([{ kind: "dog", n: 2 }, { kind: "cat", n: 2 }]);
});

test("a column the table lacks is a type error and a runtime error naming its columns", () => {
  const t = table(pets);
  // @ts-expect-error: Pet has no column nme
  expect(() => t.filter(col("nme").eq("rex"))).toThrow('filter: the table has no column "nme"; its columns are name, kind, age, owner, tags');
  // @ts-expect-error: Pet has no column agee
  expect(() => t.sort("agee")).toThrow('sort: the table has no column "agee"');
  // @ts-expect-error: the selected table has only name
  expect(() => t.select("name").select("age")).toThrow('select: the table has no column "age"; its columns are name');
  // @ts-expect-error: name is not a list column
  t.explode("name");
  // @ts-expect-error: the lazy table has no column nme
  expect(() => table.lazy(pets).groupBy("nme")).toThrow('groupBy: the table has no column "nme"');
  expect(() => table.lazy(pets).select("name").filter(col("age" as "name").gt(1))).toThrow('filter: the table has no column "age"; its columns are name');
  // An empty table knows no columns, so it checks none.
  expect(table<Pet>([]).filter(col("name").eq("x")).length).toBe(0);
});

test("gitLog reads this repository as a LazyTable, a head becoming --max-count", async () => {
  const log = gitLog({ cwd: import.meta.dir, limit: 5 });
  expect(log.explain()).toBe(`gitLog(${JSON.stringify({ cwd: import.meta.dir, limit: 5 })})`);
  const rows = await log;
  expect(rows.length).toBe(5);
  expect(rows[0]!.hash).toMatch(/^[0-9a-f]{40}$/);
  expect(Array.isArray(rows[0]!.files)).toBe(true);
  expect(typeof rows[0]!.date).toBe("number");
  expect((await gitLog({ cwd: import.meta.dir }).head(2)).length).toBe(2);
  const byAuthor = await gitLog({ cwd: import.meta.dir, limit: 5 }).explode("files").groupBy("author").agg(col("files").nUnique().alias("files"));
  expect(byAuthor.length).toBeGreaterThan(0);
  const touched: number = byAuthor[0]!.files;
  expect(touched).toBeGreaterThan(0);
  // @ts-expect-error: not an option
  expect(() => gitLog({ n: 3 })).toThrow('gitLog has no option "n": use limit');
});
