// Tables: rows queried in polars' verbs and argument order, ported from den's repl (den/repl/src/table.ts).
//
// - `Table<Row>` is eager and a real array of rows (r[0], .length, .map work; built-in map/slice return plain arrays). Its
//   verbs (filter, select, withColumns, sort, head, groupBy(...).agg(...), join, ...) run now and return new Tables.
//   Where a polars verb shares a name with an Array method, the call's arguments pick which one runs: `filter(fn)` and
//   `filter(expr)` both return a Table, `sort()`/`sort(compareFn)` and `join()`/`join(',')` are Array's, and `slice` is
//   always Array's (polars' slice(offset, length) is on LazyTable only).
// - `LazyTable<Row>` records the same verbs as a plan (`explain()`) and runs it on `await`, `collect()`, `toRecords()`,
//   `toArray(col)` or `height()`. It is thenable, resolving to the eager Table, so a cell that returns one prints its rows.
//   A scan may take a `head` right after it as a limit (gitLog's --max-count); the slice still runs after.
//
// Both check column names: a column the table lacks is a type error, and at run time an error naming its columns.
// One set of row functions below runs both, so the two never disagree.
import { nuon } from "./nuon.ts";

export type Scalar = string | number | boolean | null;
type AnyRow = Record<string, unknown>;

declare const SAME: unique symbol;
declare const LIST: unique symbol;
declare const FIELD: unique symbol;
/** The type of column `K` itself, resolved against the table's row: `col('size')` is a number in a `Table<{ size: number }>`. */
export interface Same<K extends string> { readonly [SAME]: K }
/** A list of `T` (`implode`). */
export interface ListOf<T> { readonly [LIST]: T }
/** Field `F` of the struct `T` (`struct.field`). */
export interface FieldOf<T, F extends string> { readonly [FIELD]: [T, F] }
/** An expression's value type, with its markers read against the row `R`. */
export type Resolve<R, T> =
  T extends Same<infer K> ? (K extends keyof R ? R[K] : never)
  : T extends ListOf<infer U> ? Resolve<R, U>[]
  : T extends FieldOf<infer S, infer F> ? FieldType<Resolve<R, S>, F>
  : T;
type FieldType<S, F extends string> = S extends null | undefined ? null : S extends { readonly [K in F]: infer V } ? V : never;
export type Names<R> = Extract<keyof R, string>;
type Flat<T> = { [K in keyof T]: T[K] } & {};
type UnionToIntersection<U> = (U extends unknown ? (x: U) => void : never) extends (x: infer I) => void ? I : never;

type Binary = "eq" | "neq" | "gt" | "gtEq" | "lt" | "ltEq" | "and" | "or" | "add" | "sub" | "mul" | "div";
type Unary = "not" | "isNull" | "isNotNull";
type AggFn = "sum" | "mean" | "min" | "max" | "count" | "first" | "last" | "nUnique" | "implode";
type StrFn = "contains" | "startsWith" | "endsWith" | "extract" | "toLowerCase" | "toUpperCase" | "lengths";
type ListFn = "len" | "contains" | "join";
/** An expression as data: what `col(...)...` built. */
export type ExprNode =
  | { op: "col"; name: string }
  | { op: "lit"; value: unknown }
  | { op: "alias"; name: string; arg: ExprNode }
  | { op: "binary"; fn: Binary; left: ExprNode; right: ExprNode }
  | { op: "unary"; fn: Unary; arg: ExprNode }
  | { op: "agg"; fn: AggFn; arg: ExprNode }
  | { op: "isIn"; arg: ExprNode; values: readonly unknown[] }
  | { op: "str"; fn: StrFn; arg: ExprNode; pattern?: string; literal?: boolean; group?: number }
  | { op: "list"; fn: ListFn; arg: ExprNode; value?: unknown }
  | { op: "field"; arg: ExprNode; name: string };

type Operand<M extends string> = Scalar | Expr<M, unknown, string>;
const operand = (value: Operand<string>): ExprNode => value instanceof Expr ? value.node : { op: "lit", value };

/**
 * A column expression, polars' `Expr`: `col('size').gt(1_000)`, `col('size').sum().alias('bytes')`. `N` is the columns it
 * reads, so a table refuses (as a type error) one naming a column it does not have; `T` is its value; `O` its name.
 */
export class Expr<N extends string = string, T = unknown, O extends string = string> {
  declare readonly __reads?: N;
  declare readonly __type?: T;
  declare readonly __name?: O;
  readonly node: ExprNode;
  constructor(node: ExprNode) { this.node = node; }

  private binary<M extends string, V>(fn: Binary, other: Operand<M>): Expr<N | M, V, O> { return new Expr({ op: "binary", fn, left: this.node, right: operand(other) }); }
  alias<A extends string>(name: A): Expr<N, T, A> { return new Expr({ op: "alias", name, arg: this.node }); }
  eq<M extends string = never>(other: Operand<M>) { return this.binary<M, boolean>("eq", other); }
  neq<M extends string = never>(other: Operand<M>) { return this.binary<M, boolean>("neq", other); }
  gt<M extends string = never>(other: Operand<M>) { return this.binary<M, boolean>("gt", other); }
  gtEq<M extends string = never>(other: Operand<M>) { return this.binary<M, boolean>("gtEq", other); }
  lt<M extends string = never>(other: Operand<M>) { return this.binary<M, boolean>("lt", other); }
  ltEq<M extends string = never>(other: Operand<M>) { return this.binary<M, boolean>("ltEq", other); }
  and<M extends string = never>(other: Operand<M>) { return this.binary<M, boolean>("and", other); }
  or<M extends string = never>(other: Operand<M>) { return this.binary<M, boolean>("or", other); }
  add<M extends string = never>(other: Operand<M>) { return this.binary<M, number>("add", other); }
  sub<M extends string = never>(other: Operand<M>) { return this.binary<M, number>("sub", other); }
  mul<M extends string = never>(other: Operand<M>) { return this.binary<M, number>("mul", other); }
  div<M extends string = never>(other: Operand<M>) { return this.binary<M, number>("div", other); }
  not(): Expr<N, boolean, O> { return new Expr({ op: "unary", fn: "not", arg: this.node }); }
  isNull(): Expr<N, boolean, O> { return new Expr({ op: "unary", fn: "isNull", arg: this.node }); }
  isNotNull(): Expr<N, boolean, O> { return new Expr({ op: "unary", fn: "isNotNull", arg: this.node }); }
  isIn(values: readonly Scalar[]): Expr<N, boolean, O> { return new Expr({ op: "isIn", arg: this.node, values: [...values] }); }
  private agg<V>(fn: AggFn): Expr<N, V, O> { return new Expr({ op: "agg", fn, arg: this.node }); }
  sum() { return this.agg<number>("sum"); }
  mean() { return this.agg<number | null>("mean"); }
  min() { return this.agg<T>("min"); }
  max() { return this.agg<T>("max"); }
  /** Values that are not null. */
  count() { return this.agg<number>("count"); }
  first() { return this.agg<T>("first"); }
  last() { return this.agg<T>("last"); }
  nUnique() { return this.agg<number>("nUnique"); }
  /** The group's values as one list. */
  implode() { return this.agg<ListOf<T>>("implode"); }
  /** String functions: `col('path').str.endsWith('.ts')`. */
  get str() {
    const arg = this.node;
    const make = <V>(fn: StrFn, extra: Partial<Extract<ExprNode, { op: "str" }>> = {}): Expr<N, V, O> => new Expr({ op: "str", fn, arg, ...extra });
    return {
      /** A regex match anywhere, or literal text with `literal` true (polars' `str.contains(pattern, literal)`). */
      contains: (pattern: string, literal = false) => make<boolean>("contains", { pattern, literal }),
      startsWith: (prefix: string) => make<boolean>("startsWith", { pattern: prefix }),
      endsWith: (suffix: string) => make<boolean>("endsWith", { pattern: suffix }),
      /** Capture group `group` (default 1) of the regex's first match, or null. */
      extract: (pattern: string, group = 1) => make<string | null>("extract", { pattern, group }),
      toLowerCase: () => make<string>("toLowerCase"),
      toUpperCase: () => make<string>("toUpperCase"),
      lengths: () => make<number>("lengths"),
    };
  }
  /** List functions: `col('files').list.len()`. */
  get list() {
    const arg = this.node;
    const make = <V>(fn: ListFn, value?: unknown): Expr<N, V, O> => new Expr({ op: "list", fn, arg, ...(value === undefined ? {} : { value }) });
    return {
      len: () => make<number>("len"),
      contains: (value: Scalar) => make<boolean>("contains", value),
      join: (separator: string) => make<string>("join", separator),
    };
  }
  /** Struct functions: `col('author').struct.field('name')`. */
  get struct() {
    const arg = this.node;
    return { field: <F extends string>(name: F): Expr<N, FieldOf<T, F>, F> => new Expr({ op: "field", arg, name }) };
  }
  toString(): string { return describeExpr(this.node); }
}

/** A column: `col('size')`. */
export function col<K extends string>(name: K): Expr<K, Same<K>, K> {
  if (typeof name !== "string") throw new TypeError("col(name) takes a column name");
  return new Expr({ op: "col", name });
}
/** A literal value: `lit(0)`. */
export function lit<V extends Scalar>(value: V): Expr<never, V, "literal"> { return new Expr({ op: "lit", value }); }

type Item<R> = Names<R> | Expr<Names<R>, unknown, string>;
type Out<R, E> = E extends string ? { [K in E]: K extends keyof R ? R[K] : never }
  : E extends Expr<string, infer T, infer O> ? { [K in O]: Resolve<R, T> } : never;
type Merged<R, E extends readonly unknown[]> = Flat<UnionToIntersection<Out<R, E[number]>>>;
type OutNames<E extends readonly unknown[]> = { [I in keyof E]: E[I] extends string ? E[I] : E[I] extends Expr<string, unknown, infer O> ? O : never }[number];
type ListNames<R> = { [K in Names<R>]: NonNullable<R[K]> extends readonly unknown[] ? K : never }[Names<R>];
type Element<T> = T extends readonly (infer U)[] ? U | null : T;
type Exploded<R, K extends keyof R> = Flat<Omit<R, K> & { [P in K]: Element<R[P]> }>;
type Unnested<Row, K extends keyof Row> = Omit<Row, K> & (NonNullable<Row[K]> extends object ? NonNullable<Row[K]> : never);
export type JoinHow = "inner" | "left" | "semi" | "anti";
type Joined<R, S, K extends string, H extends JoinHow, X extends string> = H extends "semi" | "anti" ? R
  : Flat<R & { [P in Exclude<Names<S>, K> as P extends keyof R ? `${P}${X}` : P]: H extends "left" ? S[P] | null : S[P] }>;
type Renamed<R, M> = Flat<{ [P in keyof R as P extends keyof M ? (M[P] extends string ? M[P] : P) : P]: R[P] }>;
type SortOptions = boolean | { descending?: boolean | readonly boolean[]; nullsLast?: boolean };
type JoinOptions<K, H, X> = { on: K | readonly K[]; how?: H; suffix?: X };

// The old aggregation builders, for `groupBy(k).agg(a => ({ n: a.count() }))`.
type NumericKeys<Row> = { [K in keyof Row]: Row[K] extends number | bigint | null | undefined ? K : never }[keyof Row];
/** One aggregation over a group of rows; T is the type it produces. */
export type Agg<Row, T> = { readonly __agg: (rows: Row[]) => T };
type AggSpec<Row> = Record<string, Agg<Row, unknown>>;
type AggOut<S> = { [P in keyof S]: S[P] extends Agg<any, infer T> ? T : never };
/** Builders handed to `agg(a => ...)`; each checks its column against Row. */
export class Aggs<Row extends object> {
  count(): Agg<Row, number> { return { __agg: (rs) => rs.length }; }
  sum<K extends NumericKeys<Row>>(k: K): Agg<Row, number> { return { __agg: (rs) => rs.reduce((s, r) => s + Number(r[k] ?? 0), 0) }; }
  mean<K extends NumericKeys<Row>>(k: K): Agg<Row, number | null> {
    return { __agg: (rs) => { const xs = rs.map((r) => r[k]).filter((x) => x != null).map(Number); return xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : null; } };
  }
  min<K extends keyof Row>(k: K): Agg<Row, Row[K] | null> { return { __agg: (rs) => reduce("min", rs.map((r) => r[k])) as Row[K] | null }; }
  max<K extends keyof Row>(k: K): Agg<Row, Row[K] | null> { return { __agg: (rs) => reduce("max", rs.map((r) => r[k])) as Row[K] | null }; }
  first<K extends keyof Row>(k: K): Agg<Row, Row[K] | undefined> { return { __agg: (rs) => rs[0]?.[k] }; }
  list<K extends keyof Row>(k: K): Agg<Row, Row[K][]> { return { __agg: (rs) => rs.map((r) => r[k]) }; }
  distinct<K extends keyof Row>(k: K): Agg<Row, Row[K][]> { return { __agg: (rs) => [...new Set(rs.map((r) => r[k]))] }; }
  /** The group's rows as a nested Table (polars implode / Nushell group-by). */
  rows(): Agg<Row, Table<Row>> { return { __agg: (rs) => table(rs) }; }
}

// ---- The row functions both forms run. ----

const describeValue = (value: unknown) => typeof value === "string" ? JSON.stringify(value) : String(value);
function describeExpr(node: ExprNode): string {
  switch (node.op) {
    case "col": return `col(${JSON.stringify(node.name)})`;
    case "lit": return `lit(${describeValue(node.value)})`;
    case "alias": return `${describeExpr(node.arg)}.alias(${JSON.stringify(node.name)})`;
    case "binary": return `${describeExpr(node.left)}.${node.fn}(${node.right.op === "lit" ? describeValue(node.right.value) : describeExpr(node.right)})`;
    case "unary": case "agg": return `${describeExpr(node.arg)}.${node.fn}()`;
    case "isIn": return `${describeExpr(node.arg)}.isIn(${JSON.stringify(node.values)})`;
    case "str": return `${describeExpr(node.arg)}.str.${node.fn}(${node.pattern === undefined ? "" : JSON.stringify(node.pattern)})`;
    case "list": return `${describeExpr(node.arg)}.list.${node.fn}(${node.value === undefined ? "" : describeValue(node.value)})`;
    case "field": return `${describeExpr(node.arg)}.struct.field(${JSON.stringify(node.name)})`;
  }
}
/** The columns an expression reads. */
function reads(node: ExprNode, into = new Set<string>()): Set<string> {
  switch (node.op) {
    case "col": into.add(node.name); break;
    case "lit": break;
    case "binary": reads(node.left, into); reads(node.right, into); break;
    default: reads(node.arg, into);
  }
  return into;
}
const readsAll = (nodes: ExprNode[]) => nodes.flatMap((node) => [...reads(node)]);
const hasAgg = (node: ExprNode): boolean => node.op === "agg" || (node.op === "binary" ? hasAgg(node.left) || hasAgg(node.right) : "arg" in node && hasAgg(node.arg));
/** An expression's column name: its alias, else its leftmost column (polars' rule), else `literal`. */
function outName(node: ExprNode): string {
  switch (node.op) {
    case "alias": case "col": case "field": return node.name;
    case "lit": return "literal";
    case "binary": return outName(node.left);
    default: return outName(node.arg);
  }
}

const isNull = (value: unknown): value is null | undefined => value === null || value === undefined;
const key = (value: unknown) => JSON.stringify(value) ?? "undefined";
/** polars' order: numbers by value, strings by code unit, false before true; anything else by its JSON. */
function compare(a: unknown, b: unknown): number {
  if (typeof a === "number" && typeof b === "number") return a - b;
  if (typeof a === "bigint" && typeof b === "bigint") return a < b ? -1 : a > b ? 1 : 0;
  if (typeof a === "string" && typeof b === "string") return a < b ? -1 : a > b ? 1 : 0;
  if (typeof a === "boolean" && typeof b === "boolean") return Number(a) - Number(b);
  if (a instanceof Date && b instanceof Date) return a.getTime() - b.getTime();
  const left = key(a);
  const right = key(b);
  return left < right ? -1 : left > right ? 1 : 0;
}
const regexes = new Map<string, RegExp>();
const regex = (pattern: string) => {
  let made = regexes.get(pattern);
  if (!made) { made = new RegExp(pattern); if (regexes.size > 256) regexes.clear(); regexes.set(pattern, made); }
  return made;
};

function binary(fn: Binary, a: unknown, b: unknown): unknown {
  if (fn === "and") return a === false || b === false ? false : isNull(a) || isNull(b) ? null : Boolean(a) && Boolean(b);
  if (fn === "or") return a === true || b === true ? true : isNull(a) || isNull(b) ? null : Boolean(a) || Boolean(b);
  if (isNull(a) || isNull(b)) return null;
  switch (fn) {
    case "eq": return compare(a, b) === 0;
    case "neq": return compare(a, b) !== 0;
    case "gt": return compare(a, b) > 0;
    case "gtEq": return compare(a, b) >= 0;
    case "lt": return compare(a, b) < 0;
    case "ltEq": return compare(a, b) <= 0;
    case "add": return typeof a === "string" || typeof b === "string" ? `${String(a)}${String(b)}` : Number(a) + Number(b);
    case "sub": return Number(a) - Number(b);
    case "mul": return Number(a) * Number(b);
    case "div": return Number(a) / Number(b);
  }
}
function scalarOp(node: Exclude<ExprNode, { op: "col" | "lit" | "alias" | "binary" | "agg" }>, value: unknown): unknown {
  switch (node.op) {
    case "unary": return node.fn === "isNull" ? isNull(value) : node.fn === "isNotNull" ? !isNull(value) : isNull(value) ? null : !value;
    case "isIn": return isNull(value) ? null : node.values.some((item) => compare(item, value) === 0);
    case "field": return value && typeof value === "object" ? (value as AnyRow)[node.name] ?? null : null;
    case "list": {
      if (!Array.isArray(value)) return null;
      if (node.fn === "len") return value.length;
      if (node.fn === "contains") return value.some((item) => compare(item, node.value) === 0);
      return value.map(String).join(String(node.value));
    }
    case "str": {
      if (isNull(value)) return null;
      const text = String(value);
      const pattern = node.pattern ?? "";
      switch (node.fn) {
        case "contains": return node.literal ? text.includes(pattern) : regex(pattern).test(text);
        case "startsWith": return text.startsWith(pattern);
        case "endsWith": return text.endsWith(pattern);
        case "extract": return regex(pattern).exec(text)?.[node.group ?? 1] ?? null;
        case "toLowerCase": return text.toLowerCase();
        case "toUpperCase": return text.toUpperCase();
        case "lengths": return [...text].length;
      }
    }
  }
}
function evalRow(node: ExprNode, row: AnyRow): unknown {
  switch (node.op) {
    case "col": return row[node.name] ?? null;
    case "lit": return node.value;
    case "alias": return evalRow(node.arg, row);
    case "binary": return binary(node.fn, evalRow(node.left, row), evalRow(node.right, row));
    case "agg": throw new TypeError(`${describeExpr(node)} aggregates, so it belongs in groupBy(...).agg(...) or a select of aggregates`);
    default: return scalarOp(node, evalRow(node.arg, row));
  }
}
function reduce(fn: AggFn, values: unknown[]): unknown {
  const present = values.filter((value) => !isNull(value));
  switch (fn) {
    case "sum": return present.reduce<number>((sum, value) => sum + Number(value), 0);
    case "mean": return present.length ? present.reduce<number>((sum, value) => sum + Number(value), 0) / present.length : null;
    case "min": return present.length ? present.reduce((low, value) => compare(value, low) < 0 ? value : low) : null;
    case "max": return present.length ? present.reduce((high, value) => compare(value, high) > 0 ? value : high) : null;
    case "count": return present.length;
    case "first": return values[0] ?? null;
    case "last": return values.at(-1) ?? null;
    case "nUnique": return new Set(values.map(key)).size;
    case "implode": return values;
  }
}
/** An expression over a group's rows: an aggregate is one value, an expression with none is the list of its values (polars). */
function evalGroup(node: ExprNode, rows: AnyRow[]): unknown {
  if (!hasAgg(node)) return reads(node).size ? rows.map((row) => evalRow(node, row)) : evalRow(node, {});
  switch (node.op) {
    case "agg":
      if (hasAgg(node.arg)) throw new TypeError(`${describeExpr(node)} aggregates an aggregate`);
      return reduce(node.fn, rows.map((row) => evalRow(node.arg, row)));
    case "alias": return evalGroup(node.arg, rows);
    case "binary": {
      const side = (part: ExprNode) => {
        if (hasAgg(part) || !reads(part).size) return evalGroup(part, rows);
        throw new TypeError(`${describeExpr(node)} mixes an aggregate with a column that is not aggregated: aggregate ${describeExpr(part)} too`);
      };
      return binary(node.fn, side(node.left), side(node.right));
    }
    case "col": case "lit": return evalRow(node, {});
    default: return scalarOp(node, evalGroup(node.arg, rows));
  }
}

function filterRows(rows: readonly AnyRow[], predicate: ExprNode): AnyRow[] { return rows.filter((row) => evalRow(predicate, row) === true); }
function sortRows(rows: readonly AnyRow[], by: string[], descending: boolean[], nullsLast: boolean): AnyRow[] {
  return [...rows].sort((a, b) => {
    for (const [index, name] of by.entries()) {
      const left = a[name];
      const right = b[name];
      if (isNull(left) || isNull(right)) {
        if (isNull(left) && isNull(right)) continue;
        return (isNull(left) ? -1 : 1) * (nullsLast ? -1 : 1);
      }
      const order = compare(left, right);
      if (order) return descending[index] ? -order : order;
    }
    return 0;
  });
}
function sliceRows(rows: readonly AnyRow[], offset: number, length: number | undefined): AnyRow[] {
  const start = offset < 0 ? Math.max(0, rows.length + offset) : offset;
  return rows.slice(start, length === undefined ? undefined : start + length);
}
function project(rows: readonly AnyRow[], items: ExprNode[], keep: boolean): AnyRow[] {
  const aggregated = items.map(hasAgg);
  // Aggregates over the whole table: one row when every item is one, else broadcast to each row (polars).
  const totals = items.map((item, index) => aggregated[index] ? evalGroup(item, [...rows]) : undefined);
  if (!keep && aggregated.length && aggregated.every(Boolean)) return [Object.fromEntries(items.map((item, index) => [outName(item), totals[index]]))];
  return rows.map((row) => {
    const out: AnyRow = keep ? { ...row } : {};
    for (const [index, item] of items.entries()) out[outName(item)] = aggregated[index] ? totals[index] : evalRow(item, row);
    return out;
  });
}
function groups(rows: readonly AnyRow[], keys: string[]): AnyRow[][] {
  const found = new Map<string, AnyRow[]>();
  for (const row of rows) {
    const id = key(keys.map((name) => row[name] ?? null));
    const group = found.get(id);
    if (group) group.push(row); else found.set(id, [row]);
  }
  return [...found.values()];
}
const keysOf = (group: AnyRow[], keys: string[]): AnyRow => Object.fromEntries(keys.map((name) => [name, group[0]![name] ?? null]));
function groupRows(rows: readonly AnyRow[], keys: string[], aggs: ExprNode[]): AnyRow[] {
  return groups(rows, keys).map((group) => {
    const out = keysOf(group, keys);
    for (const agg of aggs) out[outName(agg)] = evalGroup(agg, group);
    return out;
  });
}
/** The old callback form: each group's keys and what its `Aggs` computed. */
function groupRowsFn(rows: readonly AnyRow[], keys: string[], spec: AggSpec<AnyRow>): AnyRow[] {
  return groups(rows, keys).map((group) => ({ ...keysOf(group, keys), ...Object.fromEntries(Object.entries(spec).map(([name, a]) => [name, a.__agg(group)])) }));
}
function joinRows(left: readonly AnyRow[], right: readonly AnyRow[], on: string[], how: JoinHow, suffix: string): AnyRow[] {
  const index = new Map<string, AnyRow[]>();
  for (const row of right) {
    const id = key(on.map((name) => row[name] ?? null));
    const found = index.get(id);
    if (found) found.push(row); else index.set(id, [row]);
  }
  const rightNames = [...new Set(right.flatMap((row) => Object.keys(row)))].filter((name) => !on.includes(name));
  const out: AnyRow[] = [];
  for (const row of left) {
    // A null key matches nothing, as in polars.
    const matches = on.some((name) => isNull(row[name])) ? undefined : index.get(key(on.map((name) => row[name])));
    if (how === "semi") { if (matches) out.push(row); continue; }
    if (how === "anti") { if (!matches) out.push(row); continue; }
    if (!matches && how === "inner") continue;
    for (const match of matches ?? [undefined]) {
      const joined: AnyRow = { ...row };
      for (const name of rightNames) joined[name in row ? `${name}${suffix}` : name] = match ? match[name] ?? null : null;
      out.push(joined);
    }
  }
  return out;
}
function uniqueRows(rows: readonly AnyRow[], subset: string[] | undefined, keep: "first" | "last"): AnyRow[] {
  const seen = new Map<string, AnyRow>();
  for (const row of keep === "last" ? [...rows].reverse() : rows) {
    const id = key(subset ? subset.map((name) => row[name] ?? null) : Object.entries(row));
    if (!seen.has(id)) seen.set(id, row);
  }
  const kept = [...seen.values()];
  return keep === "last" ? kept.reverse() : kept;
}
function explodeRows(rows: readonly AnyRow[], columns: string[]): AnyRow[] {
  return rows.flatMap((row) => {
    const lists = columns.map((name) => row[name]);
    const length = Math.max(1, ...lists.map((list) => Array.isArray(list) ? list.length : 1));
    return Array.from({ length }, (_, at) => {
      const out: AnyRow = { ...row };
      for (const [index, name] of columns.entries()) {
        const list = lists[index];
        out[name] = Array.isArray(list) ? list[at] ?? null : at ? null : list ?? null;
      }
      return out;
    });
  });
}
const renameRows = (rows: readonly AnyRow[], mapping: Record<string, string>) =>
  rows.map((row) => Object.fromEntries(Object.entries(row).map(([name, value]) => [mapping[name] ?? name, value])));
const dropRows = (rows: readonly AnyRow[], gone: string[]) =>
  rows.map((row) => Object.fromEntries(Object.entries(row).filter(([name]) => !gone.includes(name))));

const list = <T>(value: T | readonly T[]): T[] => Array.isArray(value) ? [...value] : [value as T];
function sortArgs(by: string | readonly string[], options: SortOptions | undefined) {
  const keys = list<string>(by);
  const { descending = false, nullsLast = false } = typeof options === "boolean" ? { descending: options } : options ?? {};
  const flags = Array.isArray(descending) ? keys.map((_, index) => Boolean(descending[index])) : keys.map(() => Boolean(descending));
  return { keys, flags, nullsLast };
}
function joinHow(how: unknown): JoinHow {
  if (how === undefined) return "inner";
  if (how === "inner" || how === "left" || how === "semi" || how === "anti") return how;
  throw new TypeError(`join's how is inner, left, semi or anti, not ${JSON.stringify(how)}`);
}
const nodesOf = (items: readonly (string | Expr)[]) => items.map((item) => typeof item === "string" ? col(item).node : item.node);
const countNodes = (names: string[]) => names.map((name) => col(name).count().node);
const lenNode: ExprNode = { op: "alias", name: "len", arg: { op: "agg", fn: "count", arg: { op: "lit", value: 1 } } };
function renameMap(mapping: object): Record<string, string> {
  return Object.fromEntries(Object.entries(mapping).filter((entry): entry is [string, string] => typeof entry[1] === "string"));
}
const noColumn = (verb: string, name: string, have: readonly string[]) =>
  new TypeError(`${verb}: the table has no column ${JSON.stringify(name)}; its columns are ${have.join(", ")}`);

// ---- Eager: a real array. ----

/** Every column some row of `rows` has, in first-seen order. */
function columnsOf(rows: readonly object[]): string[] {
  const names = new Set<string>();
  for (const row of rows) if (row && typeof row === "object") for (const name of Object.keys(row)) names.add(name);
  return [...names];
}
/** Refuses a column no row has; an empty table knows no columns, so it checks nothing. */
function check(rows: readonly object[], names: Iterable<string>, verb: string): void {
  const have = columnsOf(rows);
  if (!have.length) return;
  for (const name of names) if (!have.includes(name)) throw noColumn(verb, name, have);
}
const raw = (rows: readonly object[]) => rows as readonly AnyRow[];
const wrap = <R extends object>(rows: readonly object[]): Table<R> => table(rows as readonly R[]);

/**
 * Rows of type `Row`: a real array (r[0], .length, .map, .slice are Array's) plus polars' verbs, which return new Tables
 * and check column names. `filter`, `sort` and `join` take polars' arguments or Array's; the arguments pick.
 */
export class Table<Row extends object> extends Array<Row> {
  // Built-in methods (map, slice, concat, ...) return plain arrays; the verbs below return Tables.
  static override get [Symbol.species]() { return Array; }

  /** Rows where `predicate` is true: `filter(col('size').gt(1_000))`, or a function of the row as Array's filter takes. */
  override filter<S extends Row>(predicate: (value: Row, index: number, array: Row[]) => value is S, thisArg?: unknown): Table<S>;
  override filter(predicate: Expr<Names<Row>, unknown, string> | ((value: Row, index: number, array: Row[]) => unknown), thisArg?: unknown): Table<Row>;
  override filter(predicate: Expr | ((value: Row, index: number, array: Row[]) => unknown), thisArg?: unknown): Table<Row> {
    if (typeof predicate === "function") return table(Array.prototype.filter.call(this, predicate, thisArg) as Row[]);
    if (!(predicate instanceof Expr)) throw new TypeError("filter takes an expression, col(name).gt(x), or a function of the row");
    check(this, reads(predicate.node), "filter");
    return wrap(filterRows(raw(this), predicate.node));
  }
  /** These columns or expressions only: `select('path', col('size').alias('bytes'))`. */
  select<const E extends readonly Item<Row>[]>(...items: E): Table<Merged<Row, E>> {
    const nodes = nodesOf(items);
    check(this, readsAll(nodes), "select");
    return wrap(project(raw(this), nodes, false));
  }
  /** Adds or replaces columns: `withColumns(col('size').div(1024).alias('kb'))`. */
  withColumns<const E extends readonly Expr<Names<Row>, unknown, string>[]>(...items: E): Table<Flat<Omit<Row, OutNames<E>> & Merged<Row, E>>> {
    const nodes = items.map((item) => item.node);
    check(this, readsAll(nodes), "withColumns");
    return wrap(project(raw(this), nodes, true));
  }
  /**
   * Sorted by `by` (a column or several), stable, as a new Table: `sort('size', true)`, `sort(['a', 'b'], { descending:
   * [true, false], nullsLast: true })`; nulls first unless `nullsLast`. `sort()` and `sort(compareFn)` are Array's, in place.
   */
  override sort(compareFn?: (a: Row, b: Row) => number): this;
  override sort<K extends Names<Row>>(by: K | readonly K[], options?: SortOptions): Table<Row>;
  override sort(by?: string | readonly string[] | ((a: Row, b: Row) => number), options?: SortOptions): this | Table<Row> {
    if (by === undefined || typeof by === "function") return super.sort(by);
    const { keys, flags, nullsLast } = sortArgs(by, options);
    check(this, keys, "sort");
    return wrap(sortRows(raw(this), keys, flags, nullsLast));
  }
  /** The first `n` rows (default 10). */
  head(n = 10): Table<Row> { return table(Array.prototype.slice.call(this, 0, Math.max(0, n)) as Row[]); }
  /** The last `n` rows (default 10). */
  tail(n = 10): Table<Row> { return table(Array.prototype.slice.call(this, Math.max(0, this.length - n)) as Row[]); }
  /** Groups by `keys`; `.agg(col('x').sum().alias('s'), ...)` or `.agg(a => ({ n: a.count() }))` says what each group becomes, `.len()` counts them. */
  groupBy<K extends Names<Row>>(...keys: (K | readonly K[])[]): GroupBy<Row, K> {
    const names = keys.flatMap((item) => list<string>(item));
    check(this, names, "groupBy");
    return new GroupBy<Row, K>(this, names);
  }
  /**
   * Joined with `other` on columns both have: `join(other, { on: 'path', how: 'left' })`; a column both have that is not
   * joined on gets `suffix` (default `_right`). `join()` and `join(',')` are Array's string join.
   */
  override join(separator?: string): string;
  override join<S extends object, K extends Names<Row> & Names<S>, H extends JoinHow = "inner", X extends string = "_right">(other: readonly S[], options: JoinOptions<K, H, X>): Table<Joined<Row, S, K, H, X>>;
  override join(other?: string | readonly object[], opts?: unknown): unknown {
    if (other === undefined || typeof other === "string") return super.join(other);
    const options = opts as JoinOptions<string, unknown, string> | undefined;
    if (!Array.isArray(other) || !options) throw new TypeError("join takes another table and { on, how?, suffix? }, or a separator string");
    const on = list<string>(options.on);
    check(this, on, "join");
    check(other, on, "join (the other table)");
    return wrap(joinRows(raw(this), raw(other), on, joinHow(options.how), options.suffix ?? "_right"));
  }
  /** Rows once each, by `subset` (default every column), keeping the first or last; order is kept. `unique('a', 'b')` works too. */
  unique<K extends Names<Row>>(subset?: K | readonly K[], options?: { keep?: "first" | "last" }): Table<Row>;
  unique<K extends Names<Row>>(...columns: K[]): Table<Row>;
  unique(...args: unknown[]): Table<Row> {
    const last = args.at(-1);
    const options = last && typeof last === "object" && !Array.isArray(last) ? (args.pop() as { keep?: "first" | "last" }) : {};
    const names = args.length === 0 || (args.length === 1 && args[0] === undefined) ? undefined : args.flatMap((arg) => list(arg as string | string[]));
    if (names) check(this, names, "unique");
    return wrap(uniqueRows(raw(this), names, options.keep ?? "first"));
  }
  /** A row per item of list column(s): `explode('files')`; an empty or null list gives one row with null. */
  explode<K extends ListNames<Row>>(columns: K | readonly K[]): Table<Exploded<Row, K>> {
    const names = list<string>(columns);
    check(this, names, "explode");
    return wrap(explodeRows(raw(this), names));
  }
  /** Columns renamed: `rename({ size: 'bytes' })`. */
  rename<const M extends { readonly [K in Names<Row>]?: string }>(mapping: M): Table<Renamed<Row, M>> {
    const map = renameMap(mapping);
    check(this, Object.keys(map), "rename");
    return wrap(renameRows(raw(this), map));
  }
  /** The values that are not null, per column, as one row (polars' `count`). */
  count(): Table<{ [K in keyof Row]: number }> { return wrap(project(raw(this), countNodes(columnsOf(this)), false)); }
  /** Drop these columns. */
  drop<K extends Names<Row>>(...cols: K[]): Table<Omit<Row, K>> {
    check(this, cols, "drop");
    return wrap(dropRows(raw(this), cols));
  }
  /** Spread a record column into top-level columns (polars unnest / Nushell flatten). */
  unnest<K extends Names<Row>>(column: K): Table<Unnested<Row, K>> {
    check(this, [column], "unnest");
    return wrap(raw(this).map((r) => { const { [column]: inner, ...rest } = r; return { ...rest, ...(inner as object | null ?? {}) }; }));
  }
  /** One column's values. */
  toArray<K extends Names<Row>>(column: K): Row[K][] {
    check(this, [column], "toArray");
    return Array.prototype.map.call(this, (r: Row) => r[column]) as Row[K][];
  }
  /** The rows as a plain array (for APIs that dislike subclasses). */
  toRecords(): Row[] { return [...this]; }
  /** How many rows it has. */
  height(): number { return this.length; }
  /** These rows as a LazyTable, to chain a plan on. */
  lazy(): LazyTable<Row> { return lazy(this); }

  // Old names, kept so current callers and habits work.
  /** Old name of `filter(fn)`. */
  where(predicate: Expr<Names<Row>, unknown, string> | ((row: Row, index: number) => unknown)): Table<Row> { return this.filter(predicate); }
  /** Add or replace columns computed from each row: `derive({ kb: r => r.size / 1024 })` (polars: withColumns). */
  derive<D extends Record<string, (r: Row, i: number) => unknown>>(cols: D): Table<Omit<Row, keyof D> & { [P in keyof D]: ReturnType<D[P]> }> {
    const entries = Object.entries(cols);
    return wrap(Array.prototype.map.call(this, (r: Row, i: number) => ({ ...r, ...Object.fromEntries(entries.map(([k, f]) => [k, f(r, i)])) })) as object[]);
  }
  /** Old name of `sort(col, { descending: desc, nullsLast: true })`. */
  sortBy<K extends Names<Row>>(column: K | readonly K[], opts: { desc?: boolean } = {}): Table<Row> { return this.sort(column, { descending: Boolean(opts.desc), nullsLast: true }); }
  /** Old name of `join(other, { on, how })`. */
  joinOn<S extends object, K extends Names<Row> & Names<S>, H extends JoinHow = "inner">(other: readonly S[], on: K | readonly K[], how?: H): Table<Joined<Row, S, K, H, "_right">> {
    return this.join(other, { on, ...(how ? { how } : {}) }) as Table<Joined<Row, S, K, H, "_right">>;
  }
  /** Old name of `toArray(col)`. */
  column<K extends Names<Row>>(column: K): Row[K][] { return this.toArray(column); }
  /** Old name of `toRecords()`. */
  rows(): Row[] { return [...this]; }

  override toString() { return nuon(this); }
  [Bun.inspect.custom]() { return nuon(this); }
}

/** `table.groupBy(keys)`: what each group becomes. */
export class GroupBy<R extends object, K extends Names<R>> {
  readonly #rows: readonly R[];
  readonly #keys: string[];
  constructor(rows: readonly R[], keys: string[]) { this.#rows = rows; this.#keys = keys; }
  /** One row per group: its keys and these expressions, `agg(col('size').sum().alias('bytes'))`; or the old `agg(a => ({ n: a.count() }))`. */
  agg<S extends AggSpec<R>>(spec: (a: Aggs<R>) => S): Table<Flat<Pick<R, K> & AggOut<S>>>;
  agg<const E extends readonly Expr<Names<R>, unknown, string>[]>(...aggs: E): Table<Flat<Pick<R, K> & Merged<R, E>>>;
  agg(...args: (Expr | ((a: Aggs<R>) => AggSpec<R>))[]): unknown {
    const [first] = args;
    if (typeof first === "function") return wrap(groupRowsFn(raw(this.#rows), this.#keys, first(new Aggs<R>()) as AggSpec<AnyRow>));
    const nodes = args.map((item) => { if (!(item instanceof Expr)) throw new TypeError("agg takes expressions, col(name).sum().alias('s'), or one function a => ({ n: a.count() })"); return item.node; });
    check(this.#rows, readsAll(nodes), "agg");
    return wrap(groupRows(raw(this.#rows), this.#keys, nodes));
  }
  /** One row per group with its row count as `len`. */
  len(): Table<Flat<Pick<R, K> & { len: number }>> { return wrap(groupRows(raw(this.#rows), this.#keys, [lenNode])); }
}

function fromRows<Row extends object>(rows: Iterable<Row>): Table<Row> {
  const t = new Table<Row>();
  for (const r of rows) t.push(r);
  return t;
}

// ---- Lazy: a plan, run on await. ----

/** What a scan is told by the step right after it, which it may use to stop early. The step still runs after. */
export interface ScanHints {
  /** At most this many rows, in the source's own order, are wanted. */
  limit?: number;
  signal?: AbortSignal;
}
/** A source of rows for a LazyTable. */
export interface ScanSpec<R extends object> {
  /** The scan as a plan shows it: `gitLog({"since":"1 month ago"})`. */
  describe: string;
  /** Its columns, when known before it runs, so verbs check names as they are written. */
  columns?: readonly Names<R>[];
  /** It takes a `head` right after it as `hints.limit`. */
  pushdownLimit?: boolean;
  scan(hints: ScanHints): Promise<readonly R[]> | readonly R[];
}
type ErasedScan = { describe: string; pushdownLimit?: boolean; scan(hints: ScanHints): Promise<readonly object[]> | readonly object[] };
/** A node of a LazyTable's plan. */
export type Plan =
  | { kind: "scan"; source: ErasedScan }
  | { kind: "filter"; input: Plan; predicate: ExprNode }
  | { kind: "filterFn"; input: Plan; fn: (row: AnyRow) => unknown }
  | { kind: "select"; input: Plan; items: ExprNode[] }
  | { kind: "withColumns"; input: Plan; items: ExprNode[] }
  | { kind: "sort"; input: Plan; by: string[]; descending: boolean[]; nullsLast: boolean }
  | { kind: "slice"; input: Plan; offset: number; length: number | undefined }
  | { kind: "groupBy"; input: Plan; keys: string[]; aggs: ExprNode[] }
  | { kind: "groupByFn"; input: Plan; keys: string[]; spec: AggSpec<AnyRow> }
  | { kind: "join"; input: Plan; other: Plan; on: string[]; how: JoinHow; suffix: string }
  | { kind: "unique"; input: Plan; subset: string[] | undefined; keep: "first" | "last" }
  | { kind: "explode"; input: Plan; columns: string[] }
  | { kind: "rename"; input: Plan; mapping: Record<string, string> }
  | { kind: "drop"; input: Plan; columns: string[] };

/** The plan as one line per node, the scan first. */
export function describePlan(plan: Plan): string {
  const steps: string[] = [];
  for (let node: Plan | undefined = plan; node; node = "input" in node ? node.input : undefined) {
    switch (node.kind) {
      case "scan": steps.push(node.source.describe); break;
      case "filter": steps.push(`filter(${describeExpr(node.predicate)})`); break;
      case "filterFn": steps.push("filter(<function>)"); break;
      case "select": steps.push(`select(${node.items.map(describeExpr).join(", ")})`); break;
      case "withColumns": steps.push(`withColumns(${node.items.map(describeExpr).join(", ")})`); break;
      case "sort": steps.push(`sort(${node.by.map((by, i) => `${by}${node.descending[i] ? " desc" : ""}`).join(", ")}${node.nullsLast ? ", nullsLast" : ""})`); break;
      case "slice": steps.push(`slice(${node.offset}${node.length === undefined ? "" : `, ${node.length}`})`); break;
      case "groupBy": steps.push(`groupBy(${node.keys.join(", ")}).agg(${node.aggs.map(describeExpr).join(", ")})`); break;
      case "groupByFn": steps.push(`groupBy(${node.keys.join(", ")}).agg(<function: ${Object.keys(node.spec).join(", ")}>)`); break;
      case "join": steps.push(`join(<${describePlan(node.other).split("\n").join(" | ")}>, on ${node.on.join(", ")}, ${node.how})`); break;
      case "unique": steps.push(`unique(${node.subset?.join(", ") ?? ""})`); break;
      case "explode": steps.push(`explode(${node.columns.join(", ")})`); break;
      case "rename": steps.push(`rename(${JSON.stringify(node.mapping)})`); break;
      case "drop": steps.push(`drop(${node.columns.join(", ")})`); break;
    }
  }
  return steps.reverse().join("\n");
}

/** Runs a plan in order over arrays; the scan gets a `head` right above it as a limit when it takes one. */
async function runPlan(plan: Plan, signal?: AbortSignal): Promise<AnyRow[]> {
  const chain: Plan[] = [];
  let leaf = plan;
  while (leaf.kind !== "scan") { chain.unshift(leaf); leaf = leaf.input; }
  signal?.throwIfAborted();
  const hints: ScanHints = signal ? { signal } : {};
  const first = chain[0];
  if (first?.kind === "slice" && first.offset >= 0 && first.length !== undefined && leaf.source.pushdownLimit) hints.limit = first.offset + first.length;
  let rows: AnyRow[] = [...raw(await leaf.source.scan(hints))];
  for (const step of chain) {
    signal?.throwIfAborted();
    switch (step.kind) {
      case "filter": rows = filterRows(rows, step.predicate); break;
      case "filterFn": rows = rows.filter((row) => Boolean(step.fn(row))); break;
      case "select": rows = project(rows, step.items, false); break;
      case "withColumns": rows = project(rows, step.items, true); break;
      case "sort": rows = sortRows(rows, step.by, step.descending, step.nullsLast); break;
      case "slice": rows = sliceRows(rows, step.offset, step.length); break;
      case "groupBy": rows = groupRows(rows, step.keys, step.aggs); break;
      case "groupByFn": rows = groupRowsFn(rows, step.keys, step.spec); break;
      case "join": rows = joinRows(rows, await runPlan(step.other, signal), step.on, step.how, step.suffix); break;
      case "unique": rows = uniqueRows(rows, step.subset, step.keep); break;
      case "explode": rows = explodeRows(rows, step.columns); break;
      case "rename": rows = renameRows(rows, step.mapping); break;
      case "drop": rows = dropRows(rows, step.columns); break;
    }
  }
  return rows;
}

/** The columns a plan's rows have, in order; undefined when only the rows can say. */
type Schema = string[] | undefined;
const withNames = (schema: Schema, items: ExprNode[], base: string[] = []): Schema => {
  if (!schema) return undefined;
  const names = [...base];
  for (const item of items) { const name = outName(item); if (!names.includes(name)) names.push(name); }
  return names;
};

/**
 * Rows of type `R`, lazily: polars' verbs and argument order with column names checked by type, recorded as a plan
 * (`explain()`). It runs on `await t` (or `collect()`), which gives the eager Table, or `toRecords()`, `toArray(col)`,
 * `height()`. `slice(offset, length)` is polars' here.
 */
export class LazyTable<R extends object> implements PromiseLike<Table<R>> {
  readonly #plan: Plan;
  readonly #schema: Schema;
  constructor(plan: Plan, schema: Schema) { this.#plan = plan; this.#schema = schema; }

  /** The plan, as data. */
  get plan(): Plan { return this.#plan; }
  /** Its column names, when known before it runs. */
  get columns(): string[] { return this.#schema ? [...this.#schema] : []; }
  /** The plan as text, the scan first. */
  explain(): string { return describePlan(this.#plan); }

  #check(names: Iterable<string>, verb: string): void {
    if (!this.#schema) return;
    for (const name of names) if (!this.#schema.includes(name)) throw noColumn(verb, name, this.#schema);
  }
  #next<S extends object>(plan: Plan, schema: Schema): LazyTable<S> { return new LazyTable<S>(plan, schema); }

  /** Rows where `predicate` is true: `filter(col('size').gt(1_000))`, or a function of the row. */
  filter(predicate: Expr<Names<R>, unknown, string> | ((row: R) => unknown)): LazyTable<R> {
    if (typeof predicate === "function") return this.#next({ kind: "filterFn", input: this.#plan, fn: (row) => predicate(row as unknown as R) }, this.#schema);
    if (!(predicate instanceof Expr)) throw new TypeError("filter takes an expression, col(name).gt(x), or a function of the row");
    this.#check(reads(predicate.node), "filter");
    return this.#next({ kind: "filter", input: this.#plan, predicate: predicate.node }, this.#schema);
  }
  /** These columns or expressions only: `select('path', col('size').alias('bytes'))`. */
  select<const E extends readonly Item<R>[]>(...items: E): LazyTable<Merged<R, E>> {
    const nodes = nodesOf(items);
    this.#check(readsAll(nodes), "select");
    return this.#next({ kind: "select", input: this.#plan, items: nodes }, withNames(this.#schema, nodes));
  }
  /** Adds or replaces columns: `withColumns(col('size').div(1024).alias('kb'))`. */
  withColumns<const E extends readonly Expr<Names<R>, unknown, string>[]>(...items: E): LazyTable<Flat<Omit<R, OutNames<E>> & Merged<R, E>>> {
    const nodes = items.map((item) => item.node);
    this.#check(readsAll(nodes), "withColumns");
    return this.#next({ kind: "withColumns", input: this.#plan, items: nodes }, withNames(this.#schema, nodes, this.#schema));
  }
  /** Sorted by `by` (a column or several): `sort('size', true)` or `sort('size', { descending: true, nullsLast: true })`; nulls first by default. */
  sort<K extends Names<R>>(by: K | readonly K[], options?: SortOptions): LazyTable<R> {
    const { keys, flags, nullsLast } = sortArgs(by, options);
    this.#check(keys, "sort");
    return this.#next({ kind: "sort", input: this.#plan, by: keys, descending: flags, nullsLast }, this.#schema);
  }
  /** The first `n` rows (default 10). */
  head(n = 10): LazyTable<R> { return this.slice(0, Math.max(0, n)); }
  /** The last `n` rows (default 10). */
  tail(n = 10): LazyTable<R> { return n <= 0 ? this.slice(0, 0) : this.slice(-n, n); }
  /** polars' slice: `length` rows from `offset` (negative counts from the end). */
  slice(offset: number, length?: number): LazyTable<R> {
    if (!Number.isInteger(offset) || (length !== undefined && (!Number.isInteger(length) || length < 0))) throw new RangeError("slice(offset, length) takes whole numbers, length at least 0");
    return this.#next({ kind: "slice", input: this.#plan, offset, length }, this.#schema);
  }
  /** Groups by `keys`; `.agg(...)` says what each group becomes, `.len()` counts them. */
  groupBy<K extends Names<R>>(...keys: (K | readonly K[])[]): LazyGroupBy<R, K> {
    const names = keys.flatMap((item) => list<string>(item));
    this.#check(names, "groupBy");
    return new LazyGroupBy<R, K>((aggs) => {
      if (typeof aggs === "function") return this.#next({ kind: "groupByFn", input: this.#plan, keys: names, spec: aggs(new Aggs<AnyRow>()) }, undefined);
      this.#check(readsAll(aggs), "agg");
      return this.#next({ kind: "groupBy", input: this.#plan, keys: names, aggs }, withNames(this.#schema, aggs, names));
    });
  }
  /** Joined with `other` (a LazyTable, Table or array) on columns both have: `join(other, { on: 'path', how: 'left' })`. */
  join<S extends object, K extends Names<R> & Names<S>, H extends JoinHow = "inner", X extends string = "_right">(other: LazyTable<S> | readonly S[], options: JoinOptions<K, H, X>): LazyTable<Joined<R, S, K, H, X>> {
    const theirs = other instanceof LazyTable ? other : lazy(other);
    const on = list<string>(options.on);
    this.#check(on, "join");
    theirs.#check(on, "join (the other table)");
    const how = joinHow(options.how);
    const suffix = options.suffix ?? "_right";
    const mine = this.#schema;
    const schema: Schema = mine && theirs.#schema && (how === "semi" || how === "anti" ? mine
      : [...mine, ...theirs.#schema.filter((name) => !on.includes(name)).map((name) => mine.includes(name) ? `${name}${suffix}` : name)]);
    return this.#next({ kind: "join", input: this.#plan, other: theirs.#plan, on, how, suffix }, schema);
  }
  /** Rows once each, by `subset` (default every column), keeping the first or last; order is kept. */
  unique<K extends Names<R>>(subset?: K | readonly K[], options: { keep?: "first" | "last" } = {}): LazyTable<R> {
    const names = subset === undefined ? undefined : list<string>(subset);
    if (names) this.#check(names, "unique");
    return this.#next({ kind: "unique", input: this.#plan, subset: names, keep: options.keep ?? "first" }, this.#schema);
  }
  /** A row per item of list column(s): `explode('files')`. */
  explode<K extends ListNames<R>>(columns: K | readonly K[]): LazyTable<Exploded<R, K>> {
    const names = list<string>(columns);
    this.#check(names, "explode");
    return this.#next({ kind: "explode", input: this.#plan, columns: names }, this.#schema);
  }
  /** Columns renamed: `rename({ size: 'bytes' })`. */
  rename<const M extends { readonly [K in Names<R>]?: string }>(mapping: M): LazyTable<Renamed<R, M>> {
    const map = renameMap(mapping);
    this.#check(Object.keys(map), "rename");
    return this.#next({ kind: "rename", input: this.#plan, mapping: map }, this.#schema?.map((name) => map[name] ?? name));
  }
  /** Drop these columns. */
  drop<K extends Names<R>>(...cols: K[]): LazyTable<Omit<R, K>> {
    this.#check(cols, "drop");
    return this.#next({ kind: "drop", input: this.#plan, columns: cols }, this.#schema?.filter((name) => !(cols as string[]).includes(name)));
  }
  /** The values that are not null, per column, as one row (polars' `count`). */
  count(): LazyTable<{ [K in keyof R]: number }> {
    if (!this.#schema) throw new TypeError("count needs the columns, and this table does not know them before it runs: select(col(name).count()) instead, or await it first");
    const items = countNodes(this.#schema);
    return this.#next({ kind: "select", input: this.#plan, items }, withNames(this.#schema, items));
  }

  /** Runs the plan: its rows as an eager Table. */
  async collect(options: { signal?: AbortSignal } = {}): Promise<Table<R>> { return wrap(await runPlan(this.#plan, options.signal)); }
  /** Runs the plan: its rows as a plain array. */
  async toRecords(options: { signal?: AbortSignal } = {}): Promise<R[]> { return (await runPlan(this.#plan, options.signal)) as unknown as R[]; }
  /** One column's values: `toArray('path')`. */
  async toArray<K extends Names<R>>(column: K, options: { signal?: AbortSignal } = {}): Promise<R[K][]> {
    this.#check([column], "toArray");
    return (await this.toRecords(options)).map((row) => row[column]);
  }
  /** How many rows it has. */
  async height(options: { signal?: AbortSignal } = {}): Promise<number> { return (await runPlan(this.#plan, options.signal)).length; }
  /** `await t` runs it: a cell that returns a LazyTable prints its rows. */
  then<A = Table<R>, B = never>(onfulfilled?: ((value: Table<R>) => A | PromiseLike<A>) | null, onrejected?: ((reason: unknown) => B | PromiseLike<B>) | null): Promise<A | B> {
    return this.collect().then(onfulfilled, onrejected);
  }

  toString(): string { return `LazyTable (not run yet; await it for its rows):\n${this.explain()}`; }
  [Bun.inspect.custom]() { return this.toString(); }
}

type GroupSpec = ExprNode[] | ((a: Aggs<AnyRow>) => AggSpec<AnyRow>);
/** `lazyTable.groupBy(keys)`: what each group becomes. */
export class LazyGroupBy<R extends object, K extends Names<R>> {
  readonly #make: (aggs: GroupSpec) => LazyTable<object>;
  constructor(make: (aggs: GroupSpec) => LazyTable<object>) { this.#make = make; }
  /** One row per group: its keys and these expressions, `agg(col('size').sum().alias('bytes'))`; or the old `agg(a => ({ n: a.count() }))`. */
  agg<S extends AggSpec<R>>(spec: (a: Aggs<R>) => S): LazyTable<Flat<Pick<R, K> & AggOut<S>>>;
  agg<const E extends readonly Expr<Names<R>, unknown, string>[]>(...aggs: E): LazyTable<Flat<Pick<R, K> & Merged<R, E>>>;
  agg(...args: (Expr | ((a: Aggs<R>) => AggSpec<R>))[]): unknown {
    const [first] = args;
    if (typeof first === "function") return this.#make(first as unknown as (a: Aggs<AnyRow>) => AggSpec<AnyRow>);
    return this.#make(args.map((item) => { if (!(item instanceof Expr)) throw new TypeError("agg takes expressions, col(name).sum().alias('s'), or one function a => ({ n: a.count() })"); return item.node; }));
  }
  /** One row per group with its row count as `len`. */
  len(): LazyTable<Flat<Pick<R, K> & { len: number }>> { return this.#make([lenNode]) as LazyTable<Flat<Pick<R, K> & { len: number }>>; }
}

/** A LazyTable over a scan: what gitLog and other sources build on. */
export function scanTable<R extends object>(spec: ScanSpec<R>): LazyTable<R> {
  return new LazyTable<R>({ kind: "scan", source: spec as ErasedScan }, spec.columns ? [...spec.columns] : undefined);
}

/** What `table.lazy` takes: rows, a promise of rows (sql, gitLog's own scan), a function returning either, or a LazyTable. */
export type LazySource<R extends object> = LazyTable<R> | Iterable<R> | PromiseLike<Iterable<R>> | (() => Iterable<R> | PromiseLike<Iterable<R>>);

/** Rows as a LazyTable: `table.lazy(sql\`...\`).filter(col('n').gt(3))`. A promise is awaited once; a function runs on every collect. */
function lazy<R extends object>(source: LazySource<R>, options: { describe?: string; columns?: readonly Names<R>[] } = {}): LazyTable<R> {
  if (source instanceof LazyTable) return source;
  if (typeof source === "function") {
    return scanTable<R>({ describe: options.describe ?? "rows(<function>)", ...(options.columns ? { columns: options.columns } : {}), scan: async () => [...await source()] });
  }
  if (Symbol.iterator in Object(source)) {
    const rows = [...(source as Iterable<R>)];
    const names = options.columns ?? (columnsOf(rows) as Names<R>[]);
    return scanTable<R>({ describe: options.describe ?? `rows(${rows.length})`, ...(names.length ? { columns: names } : {}), scan: () => rows });
  }
  if (source && typeof (source as PromiseLike<unknown>).then === "function") {
    let settled: Promise<R[]> | undefined;
    return scanTable<R>({ describe: options.describe ?? "rows(<promise>)", ...(options.columns ? { columns: options.columns } : {}), scan: () => (settled ??= Promise.resolve(source as PromiseLike<Iterable<R>>).then((rows) => [...rows])) });
  }
  throw new TypeError("table.lazy(source) takes rows, a promise of rows, a function returning either, or a LazyTable");
}

/** Wrap rows as a Table (copies the array, not the rows); `table.from` is the same, `table.lazy(source)` a LazyTable. */
export const table = Object.assign(fromRows, { from: fromRows, lazy });

/** One line each for the function list (src/functions/index.ts). */
export const tableDocs = {
  table: "table(rows) / table.from(rows): Table<Row> — a real array (r[0], .map, .length; slice(a, b), sort(fn) and join(',') stay Array's) plus polars verbs returning new Tables: filter(col('a').gt(1) | r => bool), select('a', col('b').alias('c')), withColumns(expr, ...), sort(by | [by], true | { descending, nullsLast }), head(n = 10), tail(n), groupBy(...keys).agg(col('x').sum().alias('s'), ...) or .agg(a => ({ n: a.count(), s: a.sum(c), m: a.mean(c), lo: a.min(c), hi: a.max(c), f: a.first(c), l: a.list(c), d: a.distinct(c), sub: a.rows() })) or .len(), join(other, { on, how: inner|left|semi|anti, suffix = '_right' }), unique(subset?, { keep: first|last }), explode(listCols), unnest(recordCol), rename({ from: 'to' }), drop(...cols), count(), toArray(col), lazy(); old names work: where(fn), derive({ c: r => v }), sortBy(col, { desc }), joinOn(other, on, how), column(col), rows(). table.lazy(rows | promise | () => promise): LazyTable<Row> — the same verbs plus polars' slice(offset, length), recorded as a plan (explain()) and run by await (an eager Table; a returned one prints its rows), collect(), toRecords(), toArray(col) or height(). A column the table lacks is a type error and a runtime error naming its columns.",
  col: "col(name), lit(value): Expr — polars expressions for filter, select, withColumns and agg: eq neq gt gtEq lt ltEq and or not add sub mul div isNull isNotNull isIn([...]) alias(name); aggregates sum mean min max count first last nUnique implode (a select of only aggregates is one row); .str.contains(re, literal?) startsWith endsWith extract(re, group = 1) toLowerCase toUpperCase lengths; .list.len() contains(v) join(sep); .struct.field(name).",
  gitLog: "gitLog({ cwd?, ref?, path?: string | string[], since?: '1 month ago', until?, author?, merges? = false, limit? }): LazyTable<{ hash; author; email; date: ms; subject; files: string[]; added; deleted }> — commits newest first, chainable before await (a head right after becomes --max-count): gitLog({ since: '1 month ago' }).explode('files').groupBy('author').agg(col('files').nUnique().alias('files')).",
};
