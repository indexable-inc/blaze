import { expect, test } from "bun:test";
import { bodyText } from "./gmail.ts";

test("cuts Gmail's quoted history, keeps the reply, decodes entities and keeps links", () => {
  const html = `<div dir="ltr">Sounds good &amp; see <a href="https://ix.dev/x">the doc</a>.<br>Thanks</div><br>
    <div class="gmail_quote"><div class="gmail_attr">On Mon, A wrote:<br></div><blockquote class="gmail_quote">old text</blockquote></div>`;
  expect(bodyText(html)).toEqual({ text: "Sounds good & see the doc <https://ix.dev/x>.\nThanks", quotedRemoved: true });
});

test("a forward keeps the forwarded message", () => {
  const html = `<div>FYI</div><div class="gmail_quote">---------- Forwarded message ---------<br>From: B<br><div>body</div></div>`;
  const r = bodyText(html);
  expect(r.quotedRemoved).toBe(false);
  expect(r.text).toContain("body");
});

test("plain text: cuts 'On ... wrote:' and trailing > lines; Outlook cut point; style dropped", () => {
  expect(bodyText("Yes.\n\nOn Tue, B <b@x> wrote:\n> earlier\n> more")).toEqual({ text: "Yes.", quotedRemoved: true });
  const outlook = `<style>p{color:red}</style><p>Reply here</p><div id="appendonsend"></div><hr><p>From: C</p><p>old</p>`;
  expect(bodyText(outlook)).toEqual({ text: "Reply here", quotedRemoved: true });
});
