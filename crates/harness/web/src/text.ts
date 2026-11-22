// Inline marks in the tutor's text: `code` and [words](https://url). Everything else is plain text, never HTML.

export type Seg = { k: "t"; s: string } | { k: "c"; s: string } | { k: "a"; s: string; url: string };

const MARK = /`([^`]+)`|\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g;

export function inline(text: string): Seg[] {
  const out: Seg[] = [];
  let last = 0;
  for (const m of text.matchAll(MARK)) {
    const at = m.index ?? 0;
    if (at > last) out.push({ k: "t", s: text.slice(last, at) });
    if (m[1] !== undefined) out.push({ k: "c", s: m[1] });
    else out.push({ k: "a", s: m[2] ?? "", url: m[3] ?? "" });
    last = at + m[0].length;
  }
  if (last < text.length) out.push({ k: "t", s: text.slice(last) });
  return out;
}

// Paragraphs are split by a blank line.
export function paragraphs(text: string): string[] {
  return text
    .split(/\n\s*\n/)
    .map((p) => p.trim())
    .filter((p) => p.length > 0);
}

export function pct(x: number): string {
  return `${Math.round(Math.max(0, Math.min(1, x)) * 100)}%`;
}
