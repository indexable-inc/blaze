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

/** Shell lines the way the ix docs colour them (lib/docs/colour.ts): command, flags, strings. */
function shell(code: string): string {
	const esc = (t: string): string => t.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
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
	const html = hljs.highlight(code, { language: lang }).value;
	return lang === 'rust' ? markCalls(html) : html;
}
