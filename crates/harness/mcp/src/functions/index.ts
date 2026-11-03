// Every function the harness exposes. The MCP `ts` tool puts each one in scope by name.
import { googleSearch } from "./google.ts";
import { withHiddenPage } from "../cdp.ts";
import { table, Table } from "../table.ts";
import { nuon } from "../nuon.ts";

export const functions = { googleSearch, withHiddenPage, table, Table, nuon };

/** Shown in the MCP tool description so the model knows what it can call. */
export const docs: Record<keyof typeof functions, string> = {
  googleSearch:
    "googleSearch(query: string, opts?: { limit?: number = 10; hl?: string = 'en' }): Promise<Table<{ title; url; resolved: boolean; site; snippet }>> — Google search in a hidden tab of Dia (CDP :9222). url is the real link when resolved, else Google's /goto link (Google throttles resolving after ~2 per page); site is the displayed breadcrumb.",
  withHiddenPage:
    "withHiddenPage<T>(fn: (page) => Promise<T>): Promise<T> — open a hidden Chrome tab; page has goto(url), evaluate(fn, arg), waitForSelector(sel), url(), send(cdpMethod, params). Closed afterwards.",
  table:
    "table(rows): Table<Row> — wrap any array of objects. A Table is a real array (r[0], .map, .length work) plus verbs that return new Tables: select(...cols), drop(...cols), where(fn), derive({ col: r => v }), sortBy(col | cols, { desc }), head(n), tail(n), unique(...cols), column(col), groupBy(...cols).agg(a => ({ n: a.count(), s: a.sum(c), m: a.mean(c), lo: a.min(c), hi: a.max(c), f: a.first(c), l: a.list(c), d: a.distinct(c), sub: a.rows() })), joinOn(other, on, 'inner' | 'left'), explode(listCol), unnest(recordCol), rows(). Cells may hold nested records, lists and Tables.",
  Table: "Table — the class behind table(); use for instanceof checks.",
  nuon: "nuon(value, { maxRows? }): string — format any value the way results print: NUON, i.e. JSON with bare strings and keys, and lists of records as [[col, ...]; [v, ...], ...].",
};
