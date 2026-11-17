---
status: current
summary: A native question view and Harness-owned hover cards.
---

# Questions and hover cards

Questions are a distinct interaction, with one native `Question` view rather than an arbitrary stack of
generic controls. A quiet header identifies the request, followed by a prominent wrapping question and
optional context. Options are full-width, generous rows: label and explanation stay together, with a
subtle recommendation state. A recommendation is never an answer. Free text has a full-width field,
the existing editor/caret/IME behavior, and a clear primary action. A submitted answer becomes a compact,
readable receipt. Long option sets scroll within a bounded area. Dark/light themes use the app's roles;
no decorative animation or system-looking grey bubbles.

The REPL still owns waiting and answers. Its typed `Question` element carries `text`, `detail`,
`options: {label, detail?}[]`, optional `recommended` and `ahead`, `value`, optional `answer`, `allowText`,
and `onChoose`, `onChange`, `onSubmit` handler IDs. Choosing sends the exact label; text submission sends
the current field value; explicit confirmation may send the recommendation. Empty input without a
recommendation stays disabled. The current value must be flushed before a click submits. The existing
`ask` validation, default semantics, cancellation and late-answer routing stay authoritative.

Hover help is drawn with the same Flecs text, plate, theme and overlay placement as menus/popovers.
The existing `Tooltip` component remains the sole source. One short bounded hover delay, dismiss-on-
movement/click/key/focus loss, no pointer interception, and viewport/screen-aware placement replace
AppKit `setToolTip` paths for the main Flecs view and its floating child windows. Multi-line help separates a strong first
line from quieter supporting lines. Model metadata should avoid repeating the same effort twice.

Cost: one question lays out O(options + text), with at most 200 options and a bounded scroll region;
variable-height options are currently all laid out, rather than virtualized. It adds no idle work.
One tooltip reads the existing hit ancestry and lays out only when its identity/text changes. The floor
is the same visible text/control shaping and one overlay surface; no transcript scan or new polling loop.
Checks: native scene loading, real input/dispatch tests, REPL free-text/default/cancel behavior, and
offscreen dark/light/narrow question states plus tooltip placement and dismissal.
