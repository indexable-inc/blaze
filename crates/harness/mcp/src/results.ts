/**
 * The values the lane's finished cells returned, newest last: `$_` is the latest value that was not undefined, and
 * `repl.result('t3')` is the value of the cell whose reply said `t3 completed`. Only the last KEEP are held, and a reset
 * or a hibernation drops them with the evaluator (a held value can be large, or a handle that dies with it).
 */
const KEEP = 50;
const TASK = /^t?(\d+)$/;

export class ResultHistory {
  private readonly values = new Map<string, unknown>();
  private latest: unknown;

  /** The value a cell that did not fail returned. */
  add(task: string, value: unknown): void {
    if (value !== undefined) this.latest = value;
    this.values.delete(task);
    this.values.set(task, value);
    for (const old of this.values.keys()) {
      if (this.values.size <= KEEP) break;
      this.values.delete(old);
    }
  }

  /** The latest value that was not undefined. */
  get last(): unknown { return this.latest; }

  /** The value of one cell, by the task id its reply names (`'t3'`, or `3`); throws for one not held. */
  get(task: string | number): unknown {
    const match = TASK.exec(String(task));
    if (!match) throw new TypeError(`repl.result takes a task id such as 't3', got ${JSON.stringify(task)}`);
    const id = `t${match[1]}`;
    if (!this.values.has(id)) {
      const held = [...this.values.keys()];
      throw new Error(`No result held for ${id}: it failed, ran in another lane, or is older than the last ${KEEP} cells here${held.length ? ` (held: ${held.slice(-8).join(', ')})` : ''}`);
    }
    return this.values.get(id);
  }
}
