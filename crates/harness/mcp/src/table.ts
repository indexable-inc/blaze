// Table<Row>: a real array of rows, plus typed frame verbs whose column names are checked
// against Row, and NUON printing. Verbs return new Tables; nothing mutates.
import { nuon } from "./nuon.ts";

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
  min<K extends keyof Row>(k: K): Agg<Row, Row[K] | undefined> { return { __agg: (rs) => rs.map((r) => r[k]).reduce<Row[K] | undefined>((m, x) => (m === undefined || cmp(x, m) < 0 ? x : m), undefined) }; }
  max<K extends keyof Row>(k: K): Agg<Row, Row[K] | undefined> { return { __agg: (rs) => rs.map((r) => r[k]).reduce<Row[K] | undefined>((m, x) => (m === undefined || (x != null && cmp(x, m) > 0) ? x : m), undefined) }; }
  first<K extends keyof Row>(k: K): Agg<Row, Row[K] | undefined> { return { __agg: (rs) => rs[0]?.[k] }; }
  list<K extends keyof Row>(k: K): Agg<Row, Row[K][]> { return { __agg: (rs) => rs.map((r) => r[k]) }; }
  distinct<K extends keyof Row>(k: K): Agg<Row, Row[K][]> { return { __agg: (rs) => [...new Set(rs.map((r) => r[k]))] }; }
  /** The group's rows as a nested Table (Polars implode / Nushell group-by). */
  rows(): Agg<Row, Table<Row>> { return { __agg: (rs) => table(rs) }; }
}

type Elem<T> = T extends readonly (infer E)[] ? E : T;
type Exploded<Row, K extends keyof Row> = Omit<Row, K> & { [P in K]: Elem<Row[K]> };
type Unnested<Row, K extends keyof Row> = Omit<Row, K> & (NonNullable<Row[K]> extends object ? NonNullable<Row[K]> : never);

const cmp = (a: unknown, b: unknown): number =>
  a == null ? (b == null ? 0 : 1) : b == null ? -1 : (a as any) < (b as any) ? -1 : (a as any) > (b as any) ? 1 : 0;

export class Table<Row extends object> extends Array<Row> {
  // Built-in methods (map, filter, slice, ...) return plain arrays; the verbs below return Tables.
  static override get [Symbol.species]() { return Array; }

  /** Keep only these columns. */
  select<K extends keyof Row>(...cols: K[]): Table<Pick<Row, K>> {
    return table(this.map((r) => Object.fromEntries(cols.map((c) => [c, r[c]])) as Pick<Row, K>));
  }

  /** Drop these columns. */
  drop<K extends keyof Row>(...cols: K[]): Table<Omit<Row, K>> {
    const gone = new Set<PropertyKey>(cols);
    return table(this.map((r) => Object.fromEntries(Object.entries(r).filter(([k]) => !gone.has(k))) as Omit<Row, K>));
  }

  /** Rows matching the predicate. */
  where(pred: (r: Row, i: number) => boolean): Table<Row> { return table(this.filter(pred)); }

  /** Add or replace columns computed from each row. */
  derive<D extends Record<string, (r: Row, i: number) => unknown>>(cols: D): Table<Omit<Row, keyof D> & { [P in keyof D]: ReturnType<D[P]> }> {
    const entries = Object.entries(cols);
    return table(this.map((r, i) => ({ ...r, ...Object.fromEntries(entries.map(([k, f]) => [k, f(r, i)])) }) as Omit<Row, keyof D> & { [P in keyof D]: ReturnType<D[P]> }));
  }

  /** Stable sort by one or more columns; nulls last. */
  sortBy<K extends keyof Row>(col: K | K[], opts: { desc?: boolean } = {}): Table<Row> {
    const cols = Array.isArray(col) ? col : [col];
    const sign = opts.desc ? -1 : 1;
    return table([...this].sort((a, b) => { for (const c of cols) { const d = cmp(a[c], b[c]); if (d) return sign * d; } return 0; }));
  }

  head(n = 10): Table<Row> { return table(this.slice(0, n)); }
  tail(n = 10): Table<Row> { return table(this.slice(-n)); }

  /** First row per distinct value of the given columns (all columns if none). */
  unique<K extends keyof Row>(...cols: K[]): Table<Row> {
    const seen = new Set<string>();
    return table(this.filter((r) => { const key = JSON.stringify(cols.length ? cols.map((c) => r[c]) : r); return seen.has(key) ? false : (seen.add(key), true); }));
  }

  /** All values of one column. */
  column<K extends keyof Row>(col: K): Row[K][] { return this.map((r) => r[col]); }

  /** Group rows by key columns, then aggregate: t.groupBy("site").agg(a => ({ n: a.count() })). */
  groupBy<K extends keyof Row>(...keys: K[]) {
    const groups = new Map<string, Row[]>();
    for (const r of this) {
      const id = JSON.stringify(keys.map((k) => r[k]));
      let g = groups.get(id);
      if (!g) groups.set(id, (g = []));
      g.push(r);
    }
    return {
      agg: <S extends AggSpec<Row>>(spec: (a: Aggs<Row>) => S): Table<Pick<Row, K> & AggOut<S>> => {
        const s = spec(new Aggs<Row>());
        return table([...groups.values()].map((rows) => ({
          ...Object.fromEntries(keys.map((k) => [k, rows[0]![k]])),
          ...Object.fromEntries(Object.entries(s).map(([name, a]) => [name, a.__agg(rows)])),
        }) as Pick<Row, K> & AggOut<S>));
      },
    };
  }

  /** Join on columns with the same name in both tables (Array#join is the string one). Left join keeps unmatched left rows. */
  joinOn<R extends object, K extends keyof Row & keyof R>(other: R[], on: K | K[], how: "inner" | "left" = "inner"): Table<Row & Omit<R, K>> {
    const ks = Array.isArray(on) ? on : [on];
    const idx = new Map<string, R[]>();
    for (const r of other) { const id = JSON.stringify(ks.map((k) => r[k])); const g = idx.get(id); g ? g.push(r) : idx.set(id, [r]); }
    const out: (Row & Omit<R, K>)[] = [];
    for (const l of this) {
      const matches = idx.get(JSON.stringify(ks.map((k) => l[k])));
      if (matches) for (const m of matches) out.push({ ...m, ...l } as Row & Omit<R, K>);
      else if (how === "left") out.push(l as Row & Omit<R, K>);
    }
    return table(out);
  }

  /** One row per element of a list column (Polars/Nushell explode); empty or null lists give one row with null. */
  explode<K extends keyof Row>(col: K): Table<Exploded<Row, K>> {
    const out: Exploded<Row, K>[] = [];
    for (const r of this) {
      const v = r[col];
      const items = Array.isArray(v) ? (v.length ? v : [null]) : [v];
      for (const x of items) out.push({ ...r, [col]: x } as Exploded<Row, K>);
    }
    return table(out);
  }

  /** Spread a record column into top-level columns (Polars unnest / Nushell flatten). */
  unnest<K extends keyof Row>(col: K): Table<Unnested<Row, K>> {
    return table(this.map((r) => { const { [col]: inner, ...rest } = r; return { ...rest, ...(inner as object | null ?? {}) } as Unnested<Row, K>; }));
  }

  /** Plain array copy (for APIs that dislike subclasses). */
  rows(): Row[] { return [...this]; }

  toString() { return nuon(this); }
  [Bun.inspect.custom]() { return nuon(this); }
}

/** Wrap rows as a Table (copies the array, not the rows). */
export function table<Row extends object>(rows: Iterable<Row>): Table<Row> {
  const t = new Table<Row>();
  for (const r of rows) t.push(r);
  return t;
}
