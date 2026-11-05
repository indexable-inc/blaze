// The words /bench uses for what the history names by key.

export const REPO = 'https://github.com/indexable-inc/blaze';

export const commitUrl = (commit: string) => `${REPO}/commit/${commit}`;
export const short = (commit: string) => commit.slice(0, 10);

/** "2026-10-20 16:53 UTC". */
export function when(iso: string) {
	const d = new Date(iso);
	return Number.isNaN(d.getTime()) ? iso : `${d.toISOString().slice(0, 10)} ${d.toISOString().slice(11, 16)} UTC`;
}

export const EDITS: Record<string, string> = {
	body: 'Edit a function',
	leaf: 'Edit a widely used function',
	signature: 'Change a signature',
	generic: 'Edit a generic function',
	newfn: 'Add a function',
	struct: 'Add a struct field',
	'no-op': 'Nothing changed',
	'test-edit': 'Edit, then run the tests',
	test: 'Run the tests, nothing changed',
	cold: 'Cold build, until ready',
};
export const EDIT_ORDER = ['body', 'leaf', 'signature', 'generic', 'newfn', 'struct', 'no-op', 'test-edit', 'test', 'cold'];

/** The other cargo setups measured on the same edits, as a note on the fastest one's bar. */
export const SETUPS: Record<string, string> = {
	mold: 'cargo with the mold linker',
	wild: 'cargo with the wild linker',
	threads8: 'cargo with -Zthreads=8, the parallel front end',
	nightly: 'cargo on plain nightly Rust',
	cranelift: 'cargo on nightly with the Cranelift backend',
	fastest: 'cargo with Cranelift, wild and 8 threads together',
};

export const BACKEND_NAMES: Record<string, string> = { cranelift: 'Cranelift', llvm: 'LLVM' };

export const PROGRAMS: Record<string, string> = { probe: 'probe, a small program', life: 'Game of Life', bevy: 'Bevy', egui: 'egui' };
export const PROGRAM_ORDER = ['probe', 'life', 'bevy', 'egui'];
export const MODES: Record<string, string> = {
	socket: 'One edit, sent to the daemon',
	burst: 'Typing, keystroke to running',
	replay: 'Replayed edits',
	cargo: 'Save, then blaze build',
	restart: 'An edit that needs a restart',
	'cargo-run': 'The same edits with plain cargo run',
};
export const MODE_ORDER = ['socket', 'burst', 'replay', 'cargo', 'restart', 'cargo-run'];

export const projectName = (project: string, tag: string | undefined) =>
	`${project} ${(tag ?? '').replace(/^axum-v/, '').replace(/^v/, '')}`.trim();
