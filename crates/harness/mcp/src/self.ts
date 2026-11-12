// Who this kernel is: its id in the harness index, the agent session it serves, and where harness keeps its state.

/** This kernel as the parent of its cells' objects in the harness index (`<repl>.t3`, kind repl). */
export const REPL_ID = `repl-${process.pid}-${Date.now().toString(36)}`;

/** The agent session this kernel serves (`7` for `s7`); null for a kernel run by hand. */
export const session = (): number | null => (process.env.HARNESS_SESSION ? Number(process.env.HARNESS_SESSION) : null);

/** Harness's state directory. */
export const HOME = (): string => process.env.HARNESS_HOME ?? `${process.env.HOME}/.harness`;
