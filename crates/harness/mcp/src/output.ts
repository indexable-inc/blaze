// How much of a cell's output the model reads (den's repl/src/output.ts cap): text over CAP characters keeps its first and
// last 40% (a command's verdict is usually on its last lines) around a marker that says how much was left out and names a
// file holding all of it.
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

/** Characters of a reply's output: HARNESS_REPL_OUTPUT_CAP when it is an integer of at least 200, else 20,000. */
export function outputCap(env: Record<string, string | undefined> = process.env): number {
  const value = Number(env.HARNESS_REPL_OUTPUT_CAP);
  return Number.isInteger(value) && value >= 200 ? value : 20_000;
}

const lineCount = (text: string) => text.match(/\n/g)?.length ?? 0;

/** `text` cut to the cap, with the whole of it written to `spill` when it was cut (the marker says where, or why it could not be). */
export function capped(text: string, spill: string | undefined, cap = outputCap()): string {
  if (text.length <= cap) return text;
  let where = "";
  if (spill) {
    try {
      mkdirSync(dirname(spill), { recursive: true });
      writeFileSync(spill, text, { mode: 0o600 });
      where = `; all of it (${text.length} characters, ${lineCount(text) + 1} lines) is in ${spill}: read a range with (await Bun.file(path).text()).split('\\n').slice(a, b)`;
    } catch (error) {
      where = `; writing all of it failed: ${error instanceof Error ? error.message : String(error)}`;
    }
  }
  // The marker can outgrow its 20% (a long spill path): the start and the end give up the excess.
  for (let size = Math.floor(cap * 0.4); ; ) {
    const head = text.slice(0, size);
    const tail = size > 0 ? text.slice(-size) : "";
    const omitted = text.slice(size, text.length - tail.length);
    const marker = `\n…[${omitted.length} characters, ${lineCount(omitted)} lines omitted${where}]\n`;
    const over = head.length + marker.length + tail.length - cap;
    if (over <= 0 || size === 0) return head + marker + tail;
    size = Math.max(0, size - Math.ceil(over / 2));
  }
}
