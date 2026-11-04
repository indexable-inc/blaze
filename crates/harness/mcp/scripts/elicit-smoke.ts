// Checks the "browser not on CDP" path end to end: decline, no elicitation support, accept-after-start.
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { ElicitRequestSchema } from "@modelcontextprotocol/sdk/types.js";

const DEAD = 9333;
async function run(label: string, elicit: boolean, onElicit?: (msg: string) => Promise<"accept" | "decline">) {
  const client = new Client({ name: "elicit-smoke", version: "0" }, { capabilities: elicit ? { elicitation: { form: {} } } : {} });
  if (onElicit) client.setRequestHandler(ElicitRequestSchema, async (req) => {
    console.log(`  [elicitation shown to user]\n    ${String(req.params.message).split("\n").join("\n    ")}`);
    return { action: await onElicit(String(req.params.message)), content: {} };
  });
  await client.connect(new StdioClientTransport({ command: "bun", args: ["run", `${import.meta.dir}/../src/mcp.ts`], env: { ...process.env, CDP_ENDPOINT: `http://localhost:${DEAD}` } as Record<string, string> }));
  const r = await client.callTool({ name: "ts", arguments: { code: "(await googleSearch('mcp elicitation', { limit: 2 })).select('title')" } });
  console.log(`${label}: isError=${!!r.isError}\n  ${(r.content as { text: string }[])[0]!.text.split("\n").slice(0, 4).join("\n  ")}\n`);
  await client.close();
}

// Forward DEAD -> 9222 at the TCP level, standing in for "the user started the browser".
function startForwarder() {
  return Bun.listen<{ up?: Bun.Socket<unknown>; buf: Uint8Array[] }>({
    hostname: "127.0.0.1", port: DEAD,
    socket: {
      async open(s) {
        s.data = { buf: [] };
        s.data.up = await Bun.connect({ hostname: "127.0.0.1", port: 9222, socket: { data: (_u, d) => { s.write(d); }, close: () => s.end() } });
        for (const b of s.data.buf) s.data.up.write(b);
      },
      data(s, d) { s.data.up ? s.data.up.write(d) : s.data.buf.push(new Uint8Array(d)); },
      close(s) { s.data.up?.end(); },
    },
  });
}

await run("decline", true, async () => "decline");
await run("no elicitation support", false);
let fwd: ReturnType<typeof startForwarder> | undefined;
await run("accept after starting", true, async () => { fwd ??= startForwarder(); return "accept"; });
fwd?.stop(true);
