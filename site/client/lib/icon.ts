/**
 * The few line icons the page scripts draw themselves (the lesson player's Back and speaker, the intro's speaker), in
 * the stroke of the sidebar's (renderer core icons.rs).
 *
 * Under client/lib/ (not bundled as an entry).
 */

const ICON = {
	back: '<path d="M15 5l-7 7 7 7"/>',
	sound: '<path d="M4 9.5h3.5L12 5.5v13l-4.5-4H4z"/><path d="M15.5 9a4 4 0 0 1 0 6M18 6.5a7.5 7.5 0 0 1 0 11"/>',
	muted: '<path d="M4 9.5h3.5L12 5.5v13l-4.5-4H4z"/><path d="M16 9.5l5 5M21 9.5l-5 5"/>',
};

export function icon(name: keyof typeof ICON): string {
	return `<svg class="icon" viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICON[name]}</svg>`;
}
