// Loaded before every test file (bunfig.toml): a state directory of the run's own, with no daemon in it.
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const home = mkdtempSync(join(tmpdir(), "harness-test-"));
process.env.HARNESS_HOME = home;
delete process.env.HARNESS_SOCK;
delete process.env.HARNESS_SESSION;
process.on("exit", () => rmSync(home, { recursive: true, force: true }));
