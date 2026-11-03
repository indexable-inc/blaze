// Every function the harness exposes. The MCP `ts` tool puts each one in scope by name.
import { googleSearch } from "./google.ts";
import { withHiddenPage } from "../cdp.ts";

export const functions = { googleSearch, withHiddenPage };

/** Shown in the MCP tool description so the model knows what it can call. */
export const docs: Record<keyof typeof functions, string> = {
  googleSearch:
    "googleSearch(query: string, opts?: { limit?: number = 10; hl?: string = 'en' }): Promise<{ title: string; url: string; snippet: string }[]> — Google search in a hidden tab of the Chrome on CDP :9222.",
  withHiddenPage:
    "withHiddenPage<T>(fn: (page) => Promise<T>): Promise<T> — open a hidden Chrome tab; page has goto(url), evaluate(fn, arg), waitForSelector(sel), url(), send(cdpMethod, params). Closed afterwards.",
};
