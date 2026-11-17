// How functions reach the person behind the agent. The MCP adapter installs an implementation (MCP elicitation);
// without one, confirm() is false and the caller's error says the person could not be asked.

export type Confirm = (message: string) => Promise<boolean>;
let impl: Confirm | undefined;

export function setConfirm(fn: Confirm | undefined) { impl = fn; }

/** Whether there is a way to ask the person at all (an MCP client that supports elicitation). */
export function canConfirm(): boolean { return impl !== undefined; }

/** Show the person a message with accept/decline; resolves true only if they accept. */
export async function confirm(message: string): Promise<boolean> {
  return impl ? impl(message) : false;
}
