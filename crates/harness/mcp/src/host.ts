// How functions reach the person behind the agent. The MCP server installs a real implementation
// (MCP elicitation); without one, confirm() returns false and callers fall back to an error message.

export type Confirm = (message: string) => Promise<boolean>;
let impl: Confirm | undefined;

export function setConfirm(fn: Confirm | undefined) { impl = fn; }

/** Whether there is a way to ask the person at all (an MCP client that supports elicitation). */
export function canConfirm(): boolean { return impl !== undefined; }

/** Show the person a message with accept/decline; resolves true only if they accept. */
export async function confirm(message: string): Promise<boolean> {
  return impl ? impl(message) : false;
}
