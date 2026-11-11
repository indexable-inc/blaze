// The addon against a live daemon, under Bun (or Node 20+ with a TypeScript loader):
//
//   bun crates/blaze-napi/test/smoke.ts
//
// It connects (starting the daemon when none listens: `blaze` beside the host program, else on
// PATH, or named in BLAZE_BIN), asks its status, builds a crate it writes to a temporary
// directory, asks twice more to time a warm request, and closes. BLAZE_HOME names another
// daemon's home. It exits 1, saying what failed.
import { mkdtempSync, mkdirSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { connect, type Response } from '../index.js';

function check(holds: boolean, what: string): void {
  if (!holds) throw new Error(`smoke: ${what}`);
}

const dir = realpathSync(mkdtempSync(join(tmpdir(), 'blaze-napi-smoke-')));
mkdirSync(join(dir, 'src'));
writeFileSync(
  join(dir, 'Cargo.toml'),
  '[package]\nname = "smoke"\nversion = "0.1.0"\nedition = "2024"\n\n[workspace]\n',
);
writeFileSync(join(dir, 'src', 'main.rs'), 'fn main() {\n    println!("smoke");\n}\n');

const client = await connect({
  within_ms: 120_000,
  ...(process.env.BLAZE_HOME ? { home: process.env.BLAZE_HOME } : {}),
  ...(process.env.BLAZE_BIN ? { blaze: process.env.BLAZE_BIN } : {}),
});
try {
  const status = await client.status({ within_ms: 10_000 });
  check(status.pid > 0, `status names no daemon: ${JSON.stringify(status)}`);
  console.log(`daemon pid ${status.pid}, up ${status.uptime_ms} ms`);

  const events: Response[] = [];
  const began = performance.now();
  const built = await client.build({ cwd: dir, within_ms: 600_000 }, (it) => events.push(it));
  console.log(`cold build: ${(performance.now() - began).toFixed(1)} ms, ${events.length} events`);
  for (const it of built.diagnostics) process.stderr.write(it.rendered);
  check(built.ok, `the build failed: ${JSON.stringify(built.errors)}`);
  check(built.built !== null && built.built.artifacts.length > 0, 'the build placed no artifact');

  // Two requests under way at once on the one connection, then a warm build alone.
  const [again, during] = await Promise.all([
    client.build({ cwd: dir, within_ms: 600_000 }),
    client.status({ within_ms: 10_000 }),
  ]);
  check(again.ok && during.pid === status.pid, 'two requests at once did not both answer');
  const warm = performance.now();
  const kept = await client.build({ cwd: dir, within_ms: 600_000 });
  console.log(`warm build: ${(performance.now() - warm).toFixed(2)} ms`);
  check(kept.ok, 'the warm build failed');

  const asked = performance.now();
  await client.status({ within_ms: 10_000 });
  console.log(`status round trip: ${(performance.now() - asked).toFixed(3)} ms`);

  // A deadline that passes is a typed refusal, and the client goes on.
  const late = await client.request({ request: 'Live', within_ms: 300 }).then(
    () => null,
    (error: { code: number; message: string }) => error,
  );
  check(late !== null && late.code === 5, `a request past its deadline ended as ${JSON.stringify(late)}`);
  check((await client.status({ within_ms: 10_000 })).pid === status.pid, 'the client did not go on');
} finally {
  await client.close();
  rmSync(dir, { recursive: true, force: true });
}
console.log('smoke: ok');
