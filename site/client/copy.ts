/** The copy button beside a command (Command in content/schema.flecs), set up again on every page (client/lib/page.ts). */
import { onPage } from './lib/page';

onPage((signal) => {
	const timers = new Set<ReturnType<typeof setTimeout>>();
	for (const button of document.querySelectorAll<HTMLButtonElement>('button[data-copy]')) {
		button.addEventListener(
			'click',
			async () => {
				await navigator.clipboard.writeText(button.dataset.copy ?? '');
				button.textContent = 'Copied';
				const t = setTimeout(() => {
					timers.delete(t);
					button.textContent = 'Copy';
				}, 1500);
				timers.add(t);
			},
			{ signal },
		);
	}
	return () => timers.forEach(clearTimeout);
});
