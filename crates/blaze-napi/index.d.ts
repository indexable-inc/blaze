// blaze's daemon from TypeScript (docs/client.md). The shapes are the agent API's
// (crates/blaze-api/src/lib.rs, schema 48) as its JSON rendering writes them, and
// blaze-client's options and outcome (crates/blaze-client/src/ask.rs), written here by hand:
// a field renamed there is renamed here in the same change.
//
// JSON's numbers: a `u64` above 2^53 loses its low bits in JS (ids and nanosecond times can
// be that large; counts and milliseconds are not). Bytes are arrays of numbers.

/** A content hash: 32 bytes. */
export type ContentId = number[];
export type Bytes = number[];

/** Words with their parts typed (blaze-report's `Text`). */
export type Text = TextSpan[];
export type TextSpan =
  | { Plain: string }
  | { Code: string }
  | { Path: string }
  | { Package: { name: string; version: string | null } }
  | { Version: string }
  | { Hash: string }
  | { Var: string }
  | { Count: number }
  | { Took: number };

/** One thing blaze says (blaze-report's `Report`); an API error is one. */
export interface Report {
  at: Text | null;
  log: string | null;
  rule: string | null;
  facts: { key: string; text: Text }[];
  fixes: { note: Text | null; lines: string[] }[];
  blocks: { all: string[]; what: string; count: number; notable: string[] } | null;
  causes: Report[];
  headline: Text;
  severity: 'Error' | 'Warning' | 'Note';
}

export type TargetKind = 'lib' | 'bin' | 'test' | 'bench' | 'example' | 'custom-build';
export type CrateType =
  | 'bin' | 'lib' | 'rlib' | 'dylib' | 'cdylib' | 'staticlib' | 'proc-macro' | 'sdylib';
export type ArtifactKind =
  | 'bin' | 'lib' | 'test' | 'example' | 'bench' | 'proc-macro' | 'custom-build' | 'doctest';
export type Level = 'Error' | 'Warning' | 'Note' | 'Help' | 'FailureNote';
export type Applicability =
  | 'MachineApplicable' | 'MaybeIncorrect' | 'HasPlaceholders' | 'Unspecified';
export type TestOutcome = 'Passed' | 'Failed' | 'Ignored' | 'Unreached';
export type NodeKind =
  | 'Manifest' | 'Fetch' | 'Script' | 'ScriptRun' | 'Macro' | 'Std' | 'Names' | 'Bodies'
  | 'Codegen' | 'Store' | 'Link' | 'Test';

export interface UnitTarget {
  doc: boolean;
  kind: TargetKind;
  name: string;
  test: boolean;
  doctest: boolean;
  edition: string;
  src_path: string;
  crate_types: CrateType[];
}

export interface UnitOf {
  id: number;
  test: boolean;
  target: UnitTarget;
  doctest: boolean;
  package_id: string;
  manifest_path: string;
}

export interface UnitStarted {
  name: string;
  verb: 'Compiling' | 'Checking' | 'Fresh';
  source: string | null;
  package: number;
  version: string;
  package_id: string;
}

export interface Span {
  file: string;
  line: number;
  label: string | null;
  column: number;
  primary: boolean;
  byte_end: number;
  end_line: number;
  byte_start: number;
  end_column: number;
  replacement: string | null;
  applicability: Applicability | null;
}

/** A query's key (blaze-query's `QueryKey`), as serde writes it. */
export type QueryKey = unknown;

export interface FactRead {
  key: QueryKey;
  what: string;
}

export interface Diagnostic {
  code: string | null;
  unit: UnitOf | null;
  facts: FactRead[];
  level: Level;
  spans: Span[];
  message: string;
  children: Diagnostic[];
  /** rustc's text for it, as rustc prints it: what `blaze` prints. */
  rendered: string;
  explanation: string | null;
}

export interface UnitProfile {
  test: boolean;
  debuginfo: number;
  opt_level: string;
  overflow_checks: boolean;
  debug_assertions: boolean;
}

export interface Artifact {
  env: [string, string][];
  kind: ArtifactKind;
  path: string;
  fresh: boolean;
  runner: string[];
  target: UnitTarget;
  content: ContentId;
  harness: boolean;
  package: string;
  profile: UnitProfile;
  features: string[];
  filenames: string[];
  executable: string | null;
  manifest_path: string;
}

export type LinkHow = 'First' | 'Spliced' | 'Unchanged' | { Full: { why: string[] } };

export interface Worked {
  link: 'Patched' | 'Placed';
  found: number;
  green: number;
  links: { how: LinkHow; path: string }[];
  reran: number;
  misses: number;
  placed: { cloned: number; copied: number; written: number };
  served: {
    why: string | null;
    bodies: 'served' | 'checked' | 'refused';
    outcome: 'served' | 'checked' | 'refused';
    package: string;
  }[];
  build_id: number | null;
  compiled: number;
  steps_us: [NodeKind, number][];
  critical_path_us: number;
}

export interface Unchecked {
  open: { unit: string; checked: string[] }[];
  tests: { why: string; unit: string; behind: string }[];
  behind: { id: number | null; why: string; unit: string; units: string[] }[];
  stopped: boolean;
}

export interface TestResult {
  name: string;
  binary: string;
  cached: {
    basis: 'Executed' | { Reached: { why: string } };
    millis: number;
    outcome: TestOutcome;
  } | null;
  millis: number;
  output: Bytes;
  outcome: TestOutcome;
  outside: boolean;
  executable: number;
}

/** The build's live model (blaze-protocol's `now::Now`), as serde writes it. */
export type Now = unknown;

export interface Status {
  pid: number;
  bugs: { at: string; kind: string; what: string; count: number; thread: string | null }[];
  build: {
    id: ContentId;
    exe: { len: number; path: string; inode: number; modified_ns: number };
  };
  views: number;
  builds: { id: number; now: Now; what: string; phase: string; spans: string[]; for_ms: number }[];
  history: string | null;
  requests: number[];
  settings: { name: string; value: string | null }[];
  downloads: {
    url: string;
    done: number;
    rate: number | null;
    what: string;
    total: number | null;
    idle_ms: number;
    elapsed_ms: number;
  }[];
  uptime_ms: number;
  budget_bytes: number;
  memory_bytes: number;
}

export interface PartChanged {
  now: ContentId | null;
  was: ContentId | null;
  kind: 'Input' | 'File' | 'Result';
  part: string;
  names: string | null;
  trivia: boolean;
}

export type RebuildCause =
  | 'First' | 'Unrecorded' | 'Collected' | 'Unshaped'
  | { Parts: { parts: number; since: number; changed: PartChanged[] } }
  | { Lost: { since: number } }
  | { Unfinished: { since: number } }
  | { Dropped: { since: number } }
  | { Beyond: { what: string; since: number } }
  | { Unnamed: { since: number } }
  | { Read: {
      now: ContentId | null;
      was: ContentId | null;
      read: string;
      names: string | null;
      reads: number;
      since: number | null;
      changed: number;
    } }
  | { Twice: { count: number } }
  | { Always: { why: string } }
  | { NotExplained: { what: string } }
  | { Failed: { why: string } };

export type Reason =
  | 'FirstTime'
  | { InputChanged: { file: boolean; input: string; trivia: boolean } }
  | { Evicted: { by: 'Memory' | 'Age' | 'Cap'; when: number } }
  | { PreviousFailed: { since: number } }
  | { Forced: { how: string } }
  | { KeyUnstable: { part: string } }
  | { Unexplained: { what: string } };

/** `why`'s answer. */
export interface Rebuilt {
  torn: string | null;
  build: number | null;
  chain: boolean;
  count: number;
  groups: { step: string; cause: RebuildCause; count: number; items: string[] }[];
  command: string | null;
  dropped: number | null;
  rebuilt: { ends: Reason[]; item: string; step: string; cause: RebuildCause }[];
  summary: Report[];
  restoring: boolean;
  workspace: string | null;
}

/**
 * One response of the API, as its JSON has it: a variant with no fields is its name, any
 * other an object of its name. The variants a build, a check, a test, `why` and `status`
 * send are typed; the rest of the API's are `{ [variant: string]: unknown }`.
 */
export type Response =
  | { Diagnostic: Diagnostic }
  | { Built: { members: number; profile: string; artifacts: Artifact[] } }
  | { Unit: UnitStarted }
  | { UnitBegan: UnitOf }
  | { UnitEnded: UnitOf }
  | { Fetched: { verb: 'Updating' | 'Downloaded'; what: string } }
  | { Work: Worked }
  | { Unchecked: Unchecked }
  | { TestResult: TestResult }
  | { TestReady: Artifact }
  | { Status: Status }
  | { Rebuilt: Rebuilt }
  | { Error: Report }
  | { Report: { unit: UnitOf | null; report: Report } }
  | { Doing: {
      on: string | null;
      now: Now;
      busy: number;
      idle: number;
      units: { id: number; doing: string }[];
      for_ms: number;
      running: string | null;
    } }
  | { Stopping: { doing: string } }
  | { [variant: string]: unknown };

/** A request of the API, as its JSON has it (`"Status"`, `{ "Cancel": { "id": 3 } }`). */
export type Request = string | { [variant: string]: unknown };

export interface ConnectOptions {
  /** The daemon's home (`BLAZE_HOME`'s meaning); the environment's when left out. */
  home?: string;
  /** The `blaze` program a daemon is started with; else beside the host program, else on PATH. */
  blaze?: string;
  /** Whether a daemon is started when none listens; true when left out. */
  start?: boolean;
  /** How long the connect may take, the daemon's start with it, in milliseconds. */
  within_ms: number;
}

/** What a build, a check or a test run is asked with (blaze-client's `Options`). */
export interface Options {
  /** The directory the command runs in. */
  cwd: string;
  /** How long the request may take before it is cancelled, in milliseconds. */
  within_ms: number;
  /** The environment the command runs with; the host process's when left out. */
  env?: [string, string][];
  /** More words of the cargo command line (`--lib`, `--locked`, `--bin`, `app`). */
  args?: string[];
  /** Whether diagnostics are rendered with colors. */
  color?: boolean;
  /** The columns diagnostics are rendered in. */
  width?: number;
  target?: string;
  /** `dev` of a build and a check, `test` of a test, when left out. */
  profile?: string;
  verbose?: number;
  features?: string[];
  /** Packages by name; the workspace's default members when empty. */
  packages?: string[];
  all_features?: boolean;
  manifest_path?: string;
  no_default_features?: boolean;
  /** A test only: the tests whose names contain one of these. */
  filter?: string[];
  /** A test only: run only the tests the change reached. */
  reached?: boolean;
  /** A test only: libtest's selection arguments. */
  test_args?: string[];
  /** A test only: run in blaze's MIR evaluator, checked for undefined behavior. */
  ub?: { seed: number | null; schedules: number | null };
}

export interface WhyOptions {
  all?: boolean;
  /** An absolute directory of the workspace asked of; the newest build's when left out. */
  cwd?: string;
  item?: string;
  within_ms: number;
  unexplained?: boolean;
}

/** What a build, a check or a test run came to (blaze-client's `Outcome`). */
export interface Outcome {
  /** The request's `Done`. */
  ok: boolean;
  work: Worked | null;
  built: { members: number; profile: string; artifacts: Artifact[] } | null;
  tests: TestResult[];
  /** The errors that ended the request. */
  errors: Report[];
  reports: { unit: UnitOf | null; report: Report }[];
  unchecked: Unchecked | null;
  diagnostics: Diagnostic[];
}

/**
 * What a promise of the client rejects with. `code`: 1 the daemon speaks another schema,
 * 2 bytes refused, 3 an unasked response, 4 the daemon hung up, 5 the deadline passed (the
 * request was cancelled), 6 a read or write failed, 7 the client was closed, 8 the daemon
 * answered with an error, 9 the connect failed, 10 no daemon listens and none was to be
 * started, 11 the daemon could not be started, 12 what was asked is no request, 13 a panic
 * inside the addon.
 */
export interface ClientError extends Error {
  code: number;
}

/** One connection to the daemon. Any number of calls may be under way at once. */
export class Client {
  private constructor();
  build(options: Options, onEvent?: (response: Response) => void): Promise<Outcome>;
  check(options: Options, onEvent?: (response: Response) => void): Promise<Outcome>;
  /** The tests run in the daemon: every one the filter selects, or with `reached` those the change reached. */
  test(options: Options, onEvent?: (response: Response) => void): Promise<Outcome>;
  why(options: WhyOptions): Promise<Rebuilt>;
  status(options: { within_ms: number }): Promise<Status>;
  /** Any request of the API, whole: its responses to `onEvent`, its `Done`'s `ok` as the answer. */
  request(
    options: { request: Request; within_ms: number },
    onEvent?: (response: Response) => void,
  ): Promise<{ ok: boolean }>;
  /** Hangs up: every call under way rejects with code 7. */
  close(): Promise<void>;
}

export function connect(options: ConnectOptions): Promise<Client>;
