// Fixtures for every screen and step state, and a small stand-in for the app so the page can be clicked through
// outside it (`?mock=<name>`). None of this runs inside the app.

import type { CourseScreen, Screen, Step, StepScreen, Up } from "./protocol";

const COURSE_ID = "bloom-filters";
const COURSE = "Bloom filters";
const LESSON = "False positives";

// A step the mock can mark: what is right, and why.
type MockStep = { step: Step; right: number | string[] | number[]; answer: string; why: string };

const READ: MockStep = {
  step: {
    kind: "read",
    title: "A yes is only a maybe",
    body:
      "A Bloom filter answers a lookup by checking `k` bits. If any of them is 0, the item was never added, and that answer is certain.\n\n" +
      "If all `k` bits are 1, the item was probably added, but other items may have set those same bits. That is a false positive. The filter never gives a false negative.",
    code:
      "fn contains(&self, item: &[u8]) -> bool {\n" +
      "    (0..self.k).all(|i| {\n" +
      "        let bit = self.hash(item, i) % self.m;\n" +
      "        self.bits[bit]\n" +
      "    })\n" +
      "}",
  },
  right: 0,
  answer: "",
  why: "",
};

const CHOICE: MockStep = {
  step: {
    kind: "choice",
    prompt: "A lookup finds all `k` bits set. What do you know about the item?",
    code: "",
    options: [
      "It is in the set",
      "It is probably in the set",
      "It is not in the set",
      "Nothing: the filter is full",
    ],
  },
  right: 1,
  answer: "It is probably in the set",
  why: "Other items may have set the same bits, so all ones is evidence, not proof. Only a zero bit gives a certain answer.",
};

const CHOICE_AGAIN: MockStep = {
  step: {
    kind: "choice",
    prompt: "The filter keeps `m` bits and `k` hashes. What happens to the false-positive rate as you add more items?",
    code: "let mut filter = Bloom::new(m, k);\nfor word in words {\n    filter.insert(word);\n}",
    options: ["It falls", "It stays the same", "It rises"],
  },
  right: 2,
  answer: "It rises",
  why: "Each insert sets more bits to 1, so a lookup for an item never added is more likely to find all of its bits already set.",
};

const FILL: MockStep = {
  step: { kind: "fill", prompt: "A Bloom filter can give a false positive, but never a false ___." },
  right: ["negative", "negatives"],
  answer: "negative",
  why: "An added item set all of its bits, and bits are never cleared, so a lookup for it always finds them set.",
};

const ORDER: MockStep = {
  step: {
    kind: "order",
    prompt: "Put the steps of an insert in order.",
    items: [
      "Hash the item with each of the `k` hash functions",
      "Reduce each hash modulo `m` to a bit position",
      "Set the bit at each position to 1",
      "Return: the item itself is never stored",
    ],
  },
  right: [0, 1, 2, 3],
  answer: "Hash, reduce modulo `m`, set the bits, return",
  why: "Each hash picks one position in the `m`-bit array. The insert only sets bits; no copy of the item is kept.",
};

const LESSON_STEPS: MockStep[] = [READ, CHOICE, FILL, ORDER, CHOICE_AGAIN];

function stepScreen(ms: MockStep, i: number, over: Partial<StepScreen> = {}): StepScreen {
  return {
    kind: "step",
    course: COURSE,
    lesson: LESSON,
    progress: i / LESSON_STEPS.length,
    again: ms === CHOICE_AGAIN,
    step: ms.step,
    picked: null,
    typed: "",
    order: ms.step.kind === "order" ? [2, 0, 3, 1] : [],
    mark: null,
    can_submit: false,
    ...over,
  };
}

const COURSE_SCREEN: CourseScreen = {
  kind: "course",
  id: COURSE_ID,
  title: COURSE,
  goal: "Know when a Bloom filter's answer is certain and when it is a guess, and size one for the error rate you can live with.",
  mastery: 0.46,
  due: 3,
  lessons: [
    { id: "promise", title: "What a Bloom filter promises", about: "A set that answers “definitely not” or “probably yes”.", state: "done", mastery: 0.92 },
    { id: "bits", title: "Bits and hashes", about: "`k` hash functions set `k` bits in an array of `m`.", state: "done", mastery: 0.8 },
    { id: "false-positives", title: LESSON, about: "Why a yes can be wrong, and how often it is.", state: "next", mastery: 0 },
    { id: "sizing", title: "Sizing a filter", about: "Choose `m` and `k` from the item count and a target error rate.", state: "later", mastery: 0 },
    { id: "deleting", title: "Deleting, and why you can't", about: "Counting filters, cuckoo filters, and what they cost.", state: "later", mastery: 0 },
  ],
};

const CONTINUE = [
  { id: COURSE_ID, title: COURSE, due: 3, mastery: 0.46 },
  { id: "rust-lifetimes", title: "Rust lifetimes", due: 0, mastery: 0.72 },
  { id: "tcp-congestion", title: "TCP congestion control", due: 5, mastery: 0.18 },
];

function mark(ms: MockStep, s: StepScreen): StepScreen {
  let right = false;
  const r = ms.right;
  if (s.step.kind === "choice") right = s.picked === r;
  else if (s.step.kind === "fill" && Array.isArray(r))
    right = r.some((a) => String(a) === s.typed.trim().toLowerCase());
  else if (s.step.kind === "order" && Array.isArray(r)) right = r.every((v, i) => v === s.order[i]);
  return { ...s, mark: { right, answer: ms.answer, why: ms.why }, can_submit: false };
}

export const fixtures: Record<string, { about: string; screen: Screen }> = {
  "start-empty": { about: "Start, nothing to continue", screen: { kind: "start", note: "", continue: [] } },
  start: { about: "Start, with three courses to continue", screen: { kind: "start", note: "", continue: CONTINUE } },
  "start-note": {
    about: "Start, after an ask failed",
    screen: { kind: "start", note: "The tutor could not be reached. Check the connection and ask again.", continue: CONTINUE },
  },
  making: {
    about: "The tutor is writing",
    screen: {
      kind: "making",
      kicker: "Writing your course",
      title: "How do Bloom filters work?",
      sub: "Planning five short lessons, starting with what a Bloom filter promises",
    },
  },
  read: { about: "A read step", screen: stepScreen(READ, 0) },
  choice: { about: "A choice, nothing picked", screen: stepScreen(CHOICE, 1) },
  "choice-picked": { about: "A choice, one picked", screen: stepScreen(CHOICE, 1, { picked: 1, can_submit: true }) },
  "choice-right": { about: "A choice, marked right", screen: mark(CHOICE, stepScreen(CHOICE, 1, { picked: 1 })) },
  "choice-wrong": { about: "A choice, marked wrong", screen: mark(CHOICE, stepScreen(CHOICE, 1, { picked: 0 })) },
  "choice-again": { about: "A choice that came back after a miss, with code", screen: stepScreen(CHOICE_AGAIN, 4) },
  fill: { about: "A fill, empty", screen: stepScreen(FILL, 2) },
  "fill-right": { about: "A fill, marked right", screen: mark(FILL, stepScreen(FILL, 2, { typed: "negative" })) },
  "fill-wrong": { about: "A fill, marked wrong", screen: mark(FILL, stepScreen(FILL, 2, { typed: "positive" })) },
  order: { about: "An order step", screen: stepScreen(ORDER, 3, { can_submit: true }) },
  "order-right": { about: "An order step, marked right", screen: mark(ORDER, stepScreen(ORDER, 3, { order: [0, 1, 2, 3] })) },
  course: { about: "A course and its lessons", screen: COURSE_SCREEN },
  done: {
    about: "A lesson done, another next",
    screen: { kind: "done", kicker: "Lesson complete", title: LESSON, score: "4 of 5 right on the first try", next: "Sizing a filter" },
  },
  "done-last": {
    about: "The last lesson done",
    screen: { kind: "done", kicker: "Course complete", title: COURSE, score: "Every lesson done, 88% mastered", next: "" },
  },
};

// The app's part, played locally: enough to click through a lesson.
export class MockApp {
  screen: Screen;
  private at: number;
  private onScreen: (s: Screen) => void;
  private timer: ReturnType<typeof setTimeout> | null = null;

  constructor(name: string, onScreen: (s: Screen) => void) {
    const f = fixtures[name];
    if (!f) throw new Error(`no fixture "${name}"`);
    this.screen = f.screen;
    this.onScreen = onScreen;
    this.at = f.screen.kind === "step" ? LESSON_STEPS.findIndex((m) => m.step === (f.screen as StepScreen).step) : 0;
  }

  private show(s: Screen) {
    this.screen = s;
    this.onScreen(s);
  }

  private stepAt(i: number) {
    this.at = i;
    const ms = LESSON_STEPS[i];
    if (ms) this.show(stepScreen(ms, i));
    else
      this.show({ kind: "done", kicker: "Lesson complete", title: LESSON, score: "4 of 5 right on the first try", next: "Sizing a filter" });
  }

  handle(up: Up): void {
    const s = this.screen;
    if (this.timer) clearTimeout(this.timer);
    switch (up.t) {
      case "ready":
      case "link":
        return;
      case "ask":
        if (up.text.toLowerCase().includes("fail")) {
          this.show({ kind: "start", note: "The tutor could not be reached. Check the connection and ask again.", continue: CONTINUE });
          return;
        }
        this.show({ kind: "making", kicker: up.course ? "Writing your course" : "Writing a lesson", title: up.text, sub: "Planning short lessons, starting with the idea underneath" });
        this.timer = setTimeout(() => this.show(COURSE_SCREEN), 1800);
        return;
      case "cancel":
        this.show({ kind: "start", note: "", continue: CONTINUE });
        return;
      case "open":
        this.show(COURSE_SCREEN);
        return;
      case "lesson":
        this.stepAt(0);
        return;
      case "back":
        if (s.kind === "step" || s.kind === "done") this.show(COURSE_SCREEN);
        else this.show({ kind: "start", note: "", continue: CONTINUE });
        return;
      case "next":
        if (s.kind === "done") {
          if (s.next) this.stepAt(0);
          else this.show(COURSE_SCREEN);
        } else if (s.kind === "step") this.stepAt(this.at + 1);
        return;
      case "pick":
        if (s.kind === "step" && s.mark === null) this.show({ ...s, picked: up.i, can_submit: true });
        return;
      case "type":
        if (s.kind === "step" && s.mark === null) this.show({ ...s, typed: up.text, can_submit: up.text.trim().length > 0 });
        return;
      case "order":
        if (s.kind === "step" && s.mark === null) this.show({ ...s, order: up.order, can_submit: true });
        return;
      case "submit": {
        const ms = LESSON_STEPS[this.at];
        if (s.kind === "step" && s.mark === null && s.can_submit && ms) this.show(mark(ms, s));
        return;
      }
    }
  }
}
