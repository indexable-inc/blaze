// The world interface's schema, version 1 (docs/v2/plan/design/world.md), as the JSON rendering
// has it: the same types as crates/blaze-world-api, field for field. The code tables below are
// index = code on the wire, in the order crates/blaze-world-api/src/codes.rs gives them.

export const VERSION = 1;

/** 2^53 - 1: the largest integer the schema carries. */
export const MOST_INT = Number.MAX_SAFE_INTEGER;

/** The deepest a message nests (maps and arrays open at once). */
export const MOST_DEPTH = 8;

/** 64 lowercase hex digits: 32 bytes. */
export type Hex32 = string;
/** A world: the digest of a tree. */
export type WorldId = Hex32;
export type ItemKey = Hex32;
export type FindingId = Hex32;
/** Relative, `/`-separated, no empty, `.` or `..` part, no NUL or `\`. */
export type Path = string;
/** An absolute directory on the serving machine. */
export type Checkout = string;

export const CALLS = ["open", "patch", "read", "check", "diff", "commit", "explain"] as const;
export type Call = (typeof CALLS)[number];

export const EDIT_WHYS = [
  "no_such_file",
  "exists",
  "out_of_bounds",
  "not_char_boundary",
  "expect_mismatch",
  "no_such_item",
  "overlap",
  "conflict",
] as const;
export type EditWhy = (typeof EDIT_WHYS)[number];

export const WIRE_CODES = [
  "cbor",
  "not_shortest",
  "indefinite",
  "unknown_key",
  "duplicate_key",
  "key_order",
  "missing_key",
  "unknown_variant",
  "version",
  "too_long",
  "too_many",
  "too_large",
  "too_deep",
  "bad_path",
  "range",
  "second_request",
  "short",
  "trailing",
] as const;
export type WireCode = (typeof WIRE_CODES)[number];

export const ENTRY_KINDS = ["file", "dir"] as const;
export type EntryKind = (typeof ENTRY_KINDS)[number];

export type Edit =
  | { kind: "write_file"; path: Path; text: string }
  | { kind: "delete_file"; path: Path }
  | { kind: "rename_file"; from: Path; to: Path }
  | { kind: "replace_range"; path: Path; start: number; end: number; text: string; expect: string }
  | { kind: "replace_item"; item: ItemKey; text: string };
export const EDIT_KINDS = ["write_file", "delete_file", "rename_file", "replace_range", "replace_item"] as const;

/** `dir` with path "" is the world's root. */
export type What = { kind: "file"; path: Path } | { kind: "item"; key: ItemKey } | { kind: "dir"; path: Path | "" };
export const WHAT_KINDS = ["file", "item", "dir"] as const;

export type Target = { kind: "path"; path: Path } | { kind: "item"; key: ItemKey };
export const TARGET_KINDS = ["path", "item"] as const;

export type Subject = { kind: "finding"; id: FindingId } | { kind: "item"; key: ItemKey };
export const SUBJECT_KINDS = ["finding", "item"] as const;

export type Request =
  | { v: 1; call: "open"; checkout: Checkout }
  | { v: 1; call: "patch"; base: WorldId; edits: Edit[] }
  | { v: 1; call: "read"; world: WorldId; what: What }
  | { v: 1; call: "check"; world: WorldId; select: Target[]; lints: boolean }
  | { v: 1; call: "diff"; from: WorldId; to: WorldId }
  | { v: 1; call: "commit"; world: WorldId; onto: Checkout; expect: WorldId }
  | { v: 1; call: "explain"; world: WorldId; subject: Subject };

/** Bytes from the item's start; line and column (1-based) for printing only. */
export type Place = { start: number; end: number; line: number; column: number };
export type Spot = { item: ItemKey; path: Path; place: Place };
export type Note = { message: string; at: Spot[] };
export type Finding = { id: FindingId; code: string; message: string; at: Spot; notes: Note[]; fix: Edit[] };
export type Entered = { names: number; heads: number; bodies: number; lints: number };
export type Entry = { name: string; kind: EntryKind };

export type Change =
  | { kind: "file_added"; path: Path }
  | { kind: "file_deleted"; path: Path }
  | { kind: "file_changed"; path: Path }
  | { kind: "item_added"; item: ItemKey; path: Path }
  | { kind: "item_removed"; item: ItemKey; path: Path }
  | { kind: "item_changed"; item: ItemKey; path: Path };
export const CHANGE_KINDS = ["file_added", "file_deleted", "file_changed", "item_added", "item_removed", "item_changed"] as const;

export type Refusal =
  | { kind: "edit"; index: number; why: EditWhy }
  | { kind: "no_such_world"; world: WorldId }
  | { kind: "not_a_checkout"; checkout: Checkout }
  | { kind: "no_such_path"; path: Path }
  | { kind: "no_such_item"; item: ItemKey }
  | { kind: "no_such_finding"; id: FindingId }
  | { kind: "wire"; at: number; what: WireCode; field: string; said: number }
  | { kind: "io"; path: Path; message: string }
  | { kind: "unsupported"; call: Call; why: string };
export const REFUSAL_KINDS = [
  "edit",
  "no_such_world",
  "not_a_checkout",
  "no_such_path",
  "no_such_item",
  "no_such_finding",
  "wire",
  "io",
  "unsupported",
] as const;

export type Answer = {
  world: WorldId;
  findings: Finding[];
  certified: boolean;
  entered: Entered;
  ms: number;
};

export type Response =
  | { v: 1; reply: "world"; world: WorldId }
  | { v: 1; reply: "refused"; refusal: Refusal }
  | { v: 1; reply: "text"; path: Path; start: number; text: string }
  | { v: 1; reply: "listing"; entries: Entry[] }
  | ({ v: 1; reply: "checked" } & Answer)
  | { v: 1; reply: "changes"; changes: Change[] }
  | { v: 1; reply: "committed"; files_written: number }
  | { v: 1; reply: "moved"; now: WorldId }
  | { v: 1; reply: "explained"; why: string; reads: string[] };
export const REPLIES = ["world", "refused", "text", "listing", "checked", "changes", "committed", "moved", "explained"] as const;
export type Reply = (typeof REPLIES)[number];

/** The bounds a decode holds to, as crates/blaze-world-api's `Limits`. */
export type Limits = {
  mostBytes: number;
  mostText: number;
  mostPath: number;
  mostEdits: number;
  mostList: number;
};

export const DEFAULT_LIMITS: Limits = {
  mostBytes: 64 << 20,
  mostText: 16 << 20,
  mostPath: 4096,
  mostEdits: 4096,
  mostList: 1 << 16,
};

/** Whether `text` is a world path. */
export function isPath(text: string): boolean {
  return (
    text.length > 0 &&
    !text.includes("\0") &&
    !text.includes("\\") &&
    text.split("/").every((part) => part !== "" && part !== "." && part !== "..")
  );
}

/** Whether `text` is a checkout. */
export function isCheckout(text: string): boolean {
  return text.startsWith("/") && !text.includes("\0") && !text.split("/").includes("..");
}
