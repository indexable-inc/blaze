// A phone (or pad, watch) with Developer Mode on, from ts: `phone.*` drives it through Xcode's `devicectl` (Core
// Device): what is connected, its screen as a picture or a recording, its apps and processes, opening a URL, its
// pasteboard, appearance, orientation, simulated location, files in an app's container, installing a build.
// devicectl's `--json-output -` is its stable interface, so every call returns its `result`, and `phone.ctl(...)`
// reaches any command not wrapped here. There is no touch input in Core Device: nothing here taps or types.
import { join } from "node:path";
import { effect } from "./effects.ts";
import { table, type Table } from "./table.ts";
import { artifactDir } from "./vfs.ts";
import { mkdir } from "node:fs/promises";

/** How long one devicectl command may take (seconds), unless the call says. */
const TIMEOUT_S = 60;

export class PhoneError extends Error {}

/** One device as `devicectl list devices` knows it. */
export type PhoneDevice = {
  name: string; model: string; os: string; id: string; udid: string | null;
  /** `connected` when a tunnel to it is up (wired or on the network), else `disconnected`. */
  state: string; paired: boolean; developerMode: boolean; platform: string;
};

let chosen: string | undefined;

/** Run `devicectl <args>` with JSON on stdout: its `result`. Rejects naming the command and devicectl's own error. */
async function ctl<T = Record<string, unknown>>(args: string[], opts: { timeoutS?: number; stdin?: string } = {}): Promise<T> {
  const cmd = ["xcrun", "devicectl", ...args, "--timeout", String(opts.timeoutS ?? TIMEOUT_S), "--json-output", "-"];
  let p;
  try {
    p = Bun.spawn(cmd, { stdin: opts.stdin === undefined ? "ignore" : new Blob([opts.stdin]), stdout: "pipe", stderr: "pipe" });
  } catch (e) {
    throw new PhoneError(`phone: devicectl could not start (${(e as Error).message}); it comes with Xcode 15 or later: install Xcode and run xcode-select`);
  }
  const [out, err, code] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text(), p.exited]);
  let doc: { result?: T; error?: { userInfo?: { NSLocalizedDescription?: { string?: string } }; code?: number; domain?: string } } | undefined;
  try { doc = JSON.parse(out); } catch { /* no JSON: the error below quotes stderr */ }
  if (code !== 0 || !doc || doc.error) {
    const why = doc?.error?.userInfo?.NSLocalizedDescription?.string ?? err.trim().split("\n").filter(Boolean).slice(-3).join(" ") ?? `exit ${code}`;
    throw new PhoneError(`phone: devicectl ${args.slice(0, 4).join(" ")}: ${why || `exit ${code}`}`);
  }
  return doc.result as T;
}

type RawDevice = {
  identifier?: string;
  deviceProperties?: { name?: string; osVersionNumber?: string; developerModeStatus?: string };
  hardwareProperties?: { marketingName?: string; udid?: string; platform?: string };
  connectionProperties?: { pairingState?: string; tunnelState?: string };
};

async function listDevices(): Promise<PhoneDevice[]> {
  const r = await ctl<{ devices?: RawDevice[] }>(["list", "devices"]);
  return (r.devices ?? []).map((d) => ({
    name: d.deviceProperties?.name ?? "",
    model: d.hardwareProperties?.marketingName ?? "",
    os: d.deviceProperties?.osVersionNumber ?? "",
    id: d.identifier ?? "",
    udid: d.hardwareProperties?.udid ?? null,
    state: d.connectionProperties?.tunnelState ?? "unknown",
    paired: d.connectionProperties?.pairingState === "paired",
    developerMode: d.deviceProperties?.developerModeStatus === "enabled",
    platform: d.hardwareProperties?.platform ?? "",
  }));
}

/** The device a call is for: the one named, else the one `phone.use` chose, else the only one connected. */
async function target(device?: string): Promise<string> {
  if (device ?? chosen) return (device ?? chosen)!;
  const all = await listDevices();
  const up = all.filter((d) => d.state === "connected");
  if (up.length === 1) return up[0]!.id;
  if (up.length === 0) {
    const known = all.filter((d) => d.paired).map((d) => d.name).join(", ") || "none paired";
    throw new PhoneError(`phone: no device is connected (known: ${known}). Unlock it and plug it in, or put it on this Mac's network, with Developer Mode on (Settings, Privacy & Security)`);
  }
  throw new PhoneError(`phone: ${up.length} devices are connected (${up.map((d) => d.name).join(", ")}): pass { device } or call phone.use(name)`);
}

type On = { device?: string };
const dev = async (o?: On) => ["--device", await target(o?.device)];
const artifact = async (name: string) => { const dir = artifactDir(); await mkdir(dir, { recursive: true }); return join(dir, name); };
const stamp = () => new Date().toISOString().replace(/[:.]/g, "-");

export const phone = {
  /** Every device Core Device knows, connected or not. */
  devices: (): Promise<Table<PhoneDevice>> =>
    effect("list", "Listing devices", "Listed devices", "devicectl", async () => table(await listDevices()), (t) => ({ detail: `${t.length} devices` })),
  /** Every later call without `{ device }` is for `device` (a name, UDID or identifier); nothing to clear it. */
  use(device?: string): void { chosen = device; },
  /** Any devicectl command: `await phone.ctl('device', 'info', 'details', '--device', name)`; its JSON `result`. */
  ctl: (...args: string[]): Promise<Record<string, unknown>> =>
    effect("exec", "Asking the device", "Asked the device", `devicectl ${args.slice(0, 3).join(" ")}`, () => ctl(args)),
  /** The device's own details: hardware, OS, capabilities. */
  info: async (o?: On) => ctl(["device", "info", "details", ...(await dev(o))]),
  /** Whether it is locked (a locked device refuses most calls). */
  lockState: async (o?: On) => ctl(["device", "info", "lockState", ...(await dev(o))]),
  /** Its screen now as a PNG in this chat's file space: the path, for `<Image src={path} />`. */
  screenshot: (o?: On & { name?: string }): Promise<string> =>
    effect("read", "Taking a screenshot of the device", "Took a screenshot of the device", "screen", async () => {
      const path = await artifact(o?.name ?? `phone-${stamp()}.png`);
      await ctl(["device", "capture", "screenshot", ...(await dev(o)), "--destination", path]);
      return path;
    }),
  /** `seconds` of its screen as an MP4 in this chat's file space: the path, for `<Video src={path} />`. */
  record: (seconds: number, o?: On & { name?: string }): Promise<string> =>
    effect("read", "Recording the device's screen", "Recorded the device's screen", `${seconds}s`, async () => {
      if (!(seconds > 0 && seconds <= 600)) throw new PhoneError(`phone.record: ${seconds} seconds; give 1 to 600`);
      const path = await artifact(o?.name ?? `phone-${stamp()}.mp4`);
      await ctl(["device", "capture", "screen-record", ...(await dev(o)), "--destination", path, "--duration", String(seconds)], { timeoutS: seconds + 60 });
      return path;
    }),
  /** The apps installed: Table<{ name; bundleId; version; ... }>. */
  apps: (o?: On): Promise<Table<Record<string, unknown>>> =>
    effect("list", "Listing the device's apps", "Listed the device's apps", "apps", async () => {
      const r = await ctl<{ apps?: Record<string, unknown>[] }>(["device", "info", "apps", ...(await dev(o))]);
      return table((r.apps ?? []).map((a): Record<string, unknown> => ({ name: a.name, bundleId: a.bundleIdentifier, version: a.version, ...a })));
    }, (t) => ({ detail: `${t.length} apps` })),
  /** The processes running: Table<{ pid; executable }>. */
  processes: (o?: On): Promise<Table<Record<string, unknown>>> =>
    effect("list", "Listing the device's processes", "Listed the device's processes", "processes", async () => {
      const r = await ctl<{ runningProcesses?: Record<string, unknown>[] }>(["device", "info", "processes", ...(await dev(o))]);
      return table((r.runningProcesses ?? []).map((p): Record<string, unknown> => ({ pid: p.processIdentifier, executable: p.executable })));
    }, (t) => ({ detail: `${t.length} processes` })),
  /** Launch app `bundleId` in front (`terminateExisting`: a fresh start), with `args` and `env`. */
  launch: (bundleId: string, o: On & { args?: string[]; env?: Record<string, string>; terminateExisting?: boolean } = {}) =>
    effect("exec", "Launching an app on the device", "Launched an app on the device", bundleId, async () =>
      ctl(["device", "process", "launch", ...(await dev(o)), ...(o.terminateExisting ? ["--terminate-existing"] : []),
        ...(o.env ? ["--environment-variables", JSON.stringify(o.env)] : []), bundleId, ...(o.args ?? [])])),
  /** End process `pid` (`kill`: SIGKILL). */
  terminate: async (pid: number, o: On & { kill?: boolean } = {}) =>
    ctl(["device", "process", "terminate", ...(await dev(o)), "--pid", String(pid), ...(o.kill ? ["--kill"] : [])]),
  /** Open `url` on the device: a web page in Safari, or an app's own link (`maps://`, `tel:`, a universal link). */
  openURL: (url: string, o?: On) =>
    effect("net", "Opening a link on the device", "Opened a link on the device", url, async () => ctl(["device", "process", "openURL", ...(await dev(o)), url])),
  /** Its pasteboard: `copy(text)` puts text on it, `paste()` reads its text. */
  pasteboard: {
    copy: async (text: string, o?: On) => { await ctl(["device", "pasteboard", "copy", ...(await dev(o))], { stdin: text }); },
    paste: async (o?: On): Promise<string> => {
      const p = Bun.spawn(["xcrun", "devicectl", "device", "pasteboard", "paste", ...(await dev(o)), "--timeout", String(TIMEOUT_S)], { stdout: "pipe", stderr: "pipe" });
      const [out, err, code] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text(), p.exited]);
      if (code !== 0) throw new PhoneError(`phone.pasteboard.paste: ${err.trim().split("\n").pop() ?? `exit ${code}`}`);
      return out;
    },
  },
  /** Light or dark. */
  appearance: async (mode: "light" | "dark", o?: On) => ctl(["device", "settings", "appearance", ...(await dev(o)), "--mode", mode]),
  /** Turn it: portrait, portraitUpsideDown, landscapeLeft or landscapeRight. */
  orientation: async (to: "portrait" | "portraitUpsideDown" | "landscapeLeft" | "landscapeRight", o?: On) =>
    ctl(["device", "orientation", "set", ...(await dev(o)), to]),
  /** Where it thinks it is: `location(lat, lon)` simulates a place, `location(null)` ends the simulation. */
  location: async (latitude: number | null, longitude?: number, o?: On) =>
    latitude === null
      ? ctl(["device", "simulate", "location", "clear", ...(await dev(o))])
      : ctl(["device", "simulate", "location", "coordinate", ...(await dev(o)), "--latitude", String(latitude), "--longitude", String(longitude)]),
  /** Install the app at `path` (a .app or .ipa built and signed for the device). */
  install: (path: string, o?: On) =>
    effect("write", "Installing an app on the device", "Installed an app on the device", path, async () => ctl(["device", "install", "app", ...(await dev(o)), path], { timeoutS: 600 })),
};

export const phoneDocs = {
  phone:
    "phone.* — an iPhone, iPad or Watch with Developer Mode on, through Xcode's devicectl: devices(): Table<{ name; model; os; id; state: connected|disconnected; paired; developerMode }>; screenshot(): path of a PNG of its screen now (show it: render(svelte`<Image src={p} />`, { p })); record(seconds): an MP4; apps(), processes(): Tables; launch(bundleId, { args?, env?, terminateExisting? }); terminate(pid); openURL(url) (a page in Safari, or an app link like maps://); pasteboard.copy(text) / paste(); appearance('dark'|'light'); orientation(...); location(lat, lon) / location(null); install(path); info(); lockState(); ctl(...args) for any other devicectl command (its JSON result). Calls take { device } when more than one is connected (or phone.use(name)). It cannot tap, swipe or type: Core Device has no touch input; drive an app through its URLs, launch arguments and pasteboard.",
};
