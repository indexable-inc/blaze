// Messages, version 1: the contract in ../README.md. Rust owns every screen; the page draws it and says what was done.

export type ContinueRow = { id: string; title: string; due: number; mastery: number };
export type LessonRow = { id: string; title: string; about: string; state: "done" | "next" | "later"; mastery: number };
export type Mark = { right: boolean; answer: string; why: string };

export type Step =
  | { kind: "read"; title: string; body: string; code: string }
  | { kind: "choice"; prompt: string; code: string; options: string[] }
  | { kind: "fill"; prompt: string }
  | { kind: "order"; prompt: string; items: string[] };

export type StartScreen = { kind: "start"; note: string; continue: ContinueRow[] };
export type MakingScreen = { kind: "making"; kicker: string; title: string; sub: string };
export type StepScreen = {
  kind: "step";
  course: string;
  lesson: string;
  progress: number;
  again: boolean;
  step: Step;
  picked: number | null;
  typed: string;
  order: number[];
  mark: Mark | null;
  can_submit: boolean;
};
export type CourseScreen = {
  kind: "course";
  id: string;
  title: string;
  goal: string;
  mastery: number;
  due: number;
  lessons: LessonRow[];
};
export type DoneScreen = { kind: "done"; kicker: string; title: string; score: string; next: string };

export type Screen = StartScreen | MakingScreen | StepScreen | CourseScreen | DoneScreen;

export type Down = { t: "state"; screen: Screen };

export type Up =
  | { t: "ready" }
  | { t: "ask"; text: string; course: boolean }
  | { t: "open"; course: string }
  | { t: "lesson"; course: string; lesson: string }
  | { t: "pick"; i: number }
  | { t: "type"; text: string }
  | { t: "order"; order: number[] }
  | { t: "submit" }
  | { t: "next" }
  | { t: "back" }
  | { t: "cancel" }
  | { t: "link"; url: string };

// A message the page cannot draw. Shown on the page, never ignored.
export class BadMessage extends Error {}

type Obj = Record<string, unknown>;

function obj(v: unknown, at: string): Obj {
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw new BadMessage(`${at}: expected an object`);
  return v as Obj;
}
function str(o: Obj, k: string, at: string): string {
  const v = o[k];
  if (typeof v !== "string") throw new BadMessage(`${at}.${k}: expected a string`);
  return v;
}
function num(o: Obj, k: string, at: string): number {
  const v = o[k];
  if (typeof v !== "number" || !Number.isFinite(v)) throw new BadMessage(`${at}.${k}: expected a number`);
  return v;
}
function bool(o: Obj, k: string, at: string): boolean {
  const v = o[k];
  if (typeof v !== "boolean") throw new BadMessage(`${at}.${k}: expected a boolean`);
  return v;
}
function arr(o: Obj, k: string, at: string): unknown[] {
  const v = o[k];
  if (!Array.isArray(v)) throw new BadMessage(`${at}.${k}: expected an array`);
  return v;
}
function strs(o: Obj, k: string, at: string): string[] {
  return arr(o, k, at).map((v, i) => {
    if (typeof v !== "string") throw new BadMessage(`${at}.${k}[${i}]: expected a string`);
    return v;
  });
}
function ints(o: Obj, k: string, at: string): number[] {
  return arr(o, k, at).map((v, i) => {
    if (typeof v !== "number" || !Number.isInteger(v)) throw new BadMessage(`${at}.${k}[${i}]: expected an integer`);
    return v;
  });
}

function step(v: unknown): Step {
  const at = "screen.step";
  const o = obj(v, at);
  const kind = str(o, "kind", at);
  switch (kind) {
    case "read":
      return { kind, title: str(o, "title", at), body: str(o, "body", at), code: str(o, "code", at) };
    case "choice":
      return { kind, prompt: str(o, "prompt", at), code: str(o, "code", at), options: strs(o, "options", at) };
    case "fill":
      return { kind, prompt: str(o, "prompt", at) };
    case "order":
      return { kind, prompt: str(o, "prompt", at), items: strs(o, "items", at) };
    default:
      throw new BadMessage(`${at}.kind: unknown step "${kind}"`);
  }
}

function screen(v: unknown): Screen {
  const at = "screen";
  const o = obj(v, at);
  const kind = str(o, "kind", at);
  switch (kind) {
    case "start":
      return {
        kind,
        note: str(o, "note", at),
        continue: arr(o, "continue", at).map((r, i) => {
          const a = `${at}.continue[${i}]`;
          const ro = obj(r, a);
          return { id: str(ro, "id", a), title: str(ro, "title", a), due: num(ro, "due", a), mastery: num(ro, "mastery", a) };
        }),
      };
    case "making":
      return { kind, kicker: str(o, "kicker", at), title: str(o, "title", at), sub: str(o, "sub", at) };
    case "step": {
      const p = o["picked"];
      if (p !== null && (typeof p !== "number" || !Number.isInteger(p))) throw new BadMessage(`${at}.picked: expected an integer or null`);
      const m = o["mark"];
      let mark: Mark | null = null;
      if (m !== null) {
        const mo = obj(m, `${at}.mark`);
        mark = { right: bool(mo, "right", `${at}.mark`), answer: str(mo, "answer", `${at}.mark`), why: str(mo, "why", `${at}.mark`) };
      }
      return {
        kind,
        course: str(o, "course", at),
        lesson: str(o, "lesson", at),
        progress: num(o, "progress", at),
        again: bool(o, "again", at),
        step: step(o["step"]),
        picked: p,
        typed: str(o, "typed", at),
        order: ints(o, "order", at),
        mark,
        can_submit: bool(o, "can_submit", at),
      };
    }
    case "course":
      return {
        kind,
        id: str(o, "id", at),
        title: str(o, "title", at),
        goal: str(o, "goal", at),
        mastery: num(o, "mastery", at),
        due: num(o, "due", at),
        lessons: arr(o, "lessons", at).map((r, i) => {
          const a = `${at}.lessons[${i}]`;
          const ro = obj(r, a);
          const state = str(ro, "state", a);
          if (state !== "done" && state !== "next" && state !== "later") throw new BadMessage(`${a}.state: unknown state "${state}"`);
          return { id: str(ro, "id", a), title: str(ro, "title", a), about: str(ro, "about", a), state, mastery: num(ro, "mastery", a) };
        }),
      };
    case "done":
      return { kind, kicker: str(o, "kicker", at), title: str(o, "title", at), score: str(o, "score", at), next: str(o, "next", at) };
    default:
      throw new BadMessage(`${at}.kind: unknown screen "${kind}"`);
  }
}

// Parse one message from the app. The host evaluates `window.harnessRecv(<json>)`, so it arrives as a value; a string
// is parsed as JSON too.
export function parseDown(raw: unknown): Down {
  let v = raw;
  if (typeof v === "string") {
    try {
      v = JSON.parse(v);
    } catch (e) {
      throw new BadMessage(`message: not JSON (${String(e)})`);
    }
  }
  const o = obj(v, "message");
  const t = str(o, "t", "message");
  if (t !== "state") throw new BadMessage(`message.t: unknown tag "${t}"`);
  return { t, screen: screen(o["screen"]) };
}

declare global {
  interface Window {
    harnessPost?: (json: string) => void;
    harnessRecv?: (msg: unknown) => void;
  }
}

export function inApp(): boolean {
  return typeof window.harnessPost === "function";
}

// Outside the app this hook, set by the mock, plays the app's part.
let local: ((up: Up) => void) | null = null;
export function setLocal(f: (up: Up) => void): void {
  local = f;
}

export function post(up: Up): void {
  const json = JSON.stringify(up);
  if (window.harnessPost) {
    window.harnessPost(json);
    return;
  }
  console.log("harnessPost", json);
  local?.(up);
}
