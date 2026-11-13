/**
 * Syntax highlighting, after ix packages/web/src/lib/highlight.ts: highlight.js core with only the grammars this page
 * renders, plus a small grammar for MIR's textual form. The emitted `.hljs-*` classes are coloured by
 * styles/doc-code.css.
 */
import hljs from 'highlight.js/lib/core';
import bash from 'highlight.js/lib/languages/bash';
import rust from 'highlight.js/lib/languages/rust';
import toml from 'highlight.js/lib/languages/ini';

hljs.registerLanguage('bash', bash);
hljs.registerLanguage('rust', rust);
hljs.registerLanguage('toml', toml);

const CALL_HEAD = /\b([A-Za-z_][\w-]*)(\s*\()/g;

/** Promote untagged call heads (`name(`) to function titles, touching only text between tags (ix highlight.ts). */
function markCalls(html: string): string {
	return html.replace(/<[^>]*>|[^<]+/g, (chunk) =>
		chunk.startsWith('<') ? chunk : chunk.replace(CALL_HEAD, '<span class="hljs-title function_">$1</span>$2')
	);
}

const esc = (t: string): string => t.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

/** Shell lines the way the ix docs colour them (lib/docs/colour.ts): command, flags, strings. */
function shell(code: string): string {
	return code
		.split('\n')
		.map((line) => {
			let command = true;
			return line.replace(/(\s+)|(#.*$)|("[^"]*"|'[^']*')|(\|\||&&|[|;&]|>>?)|(\S+)/g, (m, space, comment, quoted, op, word: string | undefined) => {
				if (space !== undefined) return m;
				if (comment !== undefined) return `<span class="hljs-comment">${esc(m)}</span>`;
				if (quoted !== undefined) return `<span class="hljs-string">${esc(m)}</span>`;
				if (op !== undefined) {
					command = true;
					return `<span class="hljs-keyword">${esc(m)}</span>`;
				}
				const w = word ?? '';
				if (/^[-+]/.test(w)) return `<span class="hljs-attr">${esc(w)}</span>`;
				if (command) {
					command = false;
					return `<span class="hljs-title function_">${esc(w)}</span>`;
				}
				return esc(w);
			});
		})
		.join('\n');
}

/** `code` as highlighted HTML. Safe to inject: highlight.js and `shell` escape every text run. */
export function highlight(code: string, lang: 'rust' | 'toml' | 'bash'): string {
	if (lang === 'bash') return shell(code);
	const { text, hints } = inlays(code);
	const html = hljs.highlight(text, { language: lang }).value;
	return withInlays(lang === 'rust' ? markCalls(html) : html, hints);
}

/**
 * Inlay hints, as an editor shows them: `let data«: Dir<ReadWrite>» = ..` in a Code block's text draws `: Dir<ReadWrite>`
 * faded after `data`, not part of the code (it is not highlighted, and a reader's copy leaves it out). Returns the code
 * without the hints and each hint with the offset in that code it sits at.
 */
function inlays(code: string): { text: string; hints: [number, string][] } {
	const hints: [number, string][] = [];
	let text = '';
	for (const [i, part] of code.split(/«([^»]*)»/).entries()) {
		if (i % 2) hints.push([text.length, part]);
		else text += part;
	}
	return { text, hints };
}

/** Puts each hint into highlighted HTML at its offset in the code's text (a tag counts nothing, an entity one). */
function withInlays(html: string, hints: [number, string][]): string {
	if (!hints.length) return html;
	let out = '';
	let at = 0;
	let next = 0;
	const put = () => {
		while (next < hints.length && hints[next]![0] === at) out += `<span class="inlay">${esc(hints[next++]![1])}</span>`;
	};
	for (let i = 0; i < html.length; ) {
		if (html[i] === '<') {
			const end = html.indexOf('>', i) + 1;
			// A hint at the end of a highlighted run goes after its closing tag, so it takes none of the run's colour.
			if (html[i + 1] !== '/') put();
			out += html.slice(i, end);
			i = end;
			continue;
		}
		put();
		const end = html[i] === '&' ? html.indexOf(';', i) + 1 : i + 1;
		out += html.slice(i, end);
		i = end;
		at++;
	}
	put();
	if (next < hints.length) throw new Error(`gen/highlight.ts: inlay hint ${JSON.stringify(hints[next]![1])} is past the code's end`);
	return out;
}
