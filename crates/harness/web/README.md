# harness web: the app's pages, drawn by CEF

The harness app's content (the start page and lessons first, then chats) is one Svelte 5 page drawn by the harness
browser (CEF, `harness-browser`) as a `ui.Browser` node filling the main pane. Rust keeps all state (the daemon link, the
model, `harness-learn`'s runs and spaced memory); the page only draws it and says what the person did. Text fields,
editing keys, input methods, selection, scrolling and accessibility are the browser's.

Kept native: the window, the sidebar, terminal panes (libghostty-vt on Metal).

## Build

    cd crates/harness/web && bun install && bun run build      # -> dist/index.html, one self-contained file
    bun run dev                                                # Vite dev server; open /?mock=<fixture> in Chrome

`dist/index.html` inlines every script, style and font (vite-plugin-singlefile), so it loads from `file://` with no
server. The app loads `$HARNESS_WEB_URL` when set (the dev server, for live reload while designing), else the
`dist/index.html` built from this directory.

## Transport

- Page to app: `window.harnessPost(json: string)`, a binding the host adds to UI views only (CDP `Runtime.addBinding`).
- App to page: the host evaluates `window.harnessRecv(<json>)` in the page.
- A UI view never navigates away: a link opens in the browser pane, through the app.
- Outside the app (`bun run dev`, `?mock=<name>`), `harnessPost` is absent: the page draws the fixture named and logs
  what it would send.

## Messages, version 1

Every message is a JSON object with a tag `t`. Unknown tags are an error the page shows (never ignored).

### App to page

`{t: "state", screen: Screen}`: the whole screen, sent on every change (it is small). The page keeps no state of its
own but what is being typed and what has focus.

```ts
type Screen =
  | { kind: "start"; note: string;                       // note: why the last ask failed, "" when none
      continue: { id: string; title: string; due: number; mastery: number }[] }   // at most 3; mastery 0..1
  | { kind: "making"; kicker: string; title: string; sub: string }               // the tutor is writing
  | { kind: "step"; course: string; lesson: string;      // titles
      progress: number; again: boolean;                  // 0..1; this check came back after a miss
      step: Step; picked: number | null; typed: string; order: number[];
      mark: null | { right: boolean; answer: string; why: string }; can_submit: boolean }
  | { kind: "course"; id: string; title: string; goal: string; mastery: number; due: number;
      lessons: { id: string; title: string; about: string; state: "done" | "next" | "later"; mastery: number }[] }
  | { kind: "done"; kicker: string; title: string; score: string; next: string }  // next: the next lesson's title, "" when none

type Step =
  | { kind: "read"; title: string; body: string; code: string }
  | { kind: "choice"; prompt: string; code: string; options: string[] }
  | { kind: "fill"; prompt: string }                     // prompt holds "___" where the answer goes
  | { kind: "order"; prompt: string; items: string[] }   // order: indexes into items, as ordered now
```

### Page to app

```ts
type Up =
  | { t: "ready" }                          // the page loaded: send the state
  | { t: "ask"; text: string; course: boolean }   // the start page's Return (course: cmd+Return forces a lesson)
  | { t: "open"; course: string }           // a course pressed (start page's Continue, sidebar)
  | { t: "lesson"; course: string; lesson: string }  // a lesson pressed on a course's page
  | { t: "pick"; i: number }                // a choice option
  | { t: "type"; text: string }             // a fill's text, on every edit
  | { t: "order"; order: number[] }         // an order step's new order
  | { t: "submit" }                         // check the answer
  | { t: "next" }                           // continue past a read or a marked check, or past done
  | { t: "back" }                           // esc: out of a lesson to its course, out of a course to start
  | { t: "cancel" }                         // stop the tutor writing
  | { t: "link"; url: string }              // open a link in the browser pane
```

## Keys (the page's)

- Start: Return asks, cmd+Return makes a lesson of it.
- Step: 1 to 9 pick an option; Return checks, then continues; arrows move the choice; Esc goes back.
- An order step: arrows move the focused item, alt+up/down moves it.
- The app's own bindings (cmd+N, cmd+1 to 9, cmd+W, the palette) are taken before the page sees a key.

## Where version 1 is silent (the page's choices)

- Keys, filled in: on making, Esc cancels; on a course, Return opens the `next` lesson and Esc goes back; on done,
  Return keeps going (or goes back when `next` is ""), Esc goes back. In a step, Return on a focused control that is
  not one of the step's rows (the back chevron, a link) presses that control instead; arrows wrap round the options.
  On the start page Shift+Return is a new line.
- Text from the tutor is plain text with two inline marks: `` `code` `` and `[words](https://url)`; a link posts
  `link`. A read's `body` is split into paragraphs by a blank line. Never HTML.
- A marked choice finds its right option as the pick when `mark.right`, else the option whose text equals
  `mark.answer`.
- A step has no id, so what is being typed (a fill) resets when the step's course, lesson, `again` or content change.
- A message the page cannot draw (an unknown tag, a field of the wrong type) replaces the screen with the error,
  naming the field.
- Look: the colours are `00-tokens.flecs`'s `tone_*` roles as CSS variables (`src/theme.css`), dark unless the system
  is light; Inter for text and JetBrains Mono for code, both embedded from `fonts/` (OFL); no motion of any kind.
- Dev only: `/?mock` lists the fixtures (`src/mock.ts`), `?mock=<name>` draws one and plays the app's part so it can
  be clicked through, `&theme=light|dark` forces a look. `bun run check` type-checks.
