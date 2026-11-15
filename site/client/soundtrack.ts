/**
 * The intro's soundtrack, rendered off the page's thread. Not a page script: client/lib/intro-sound.ts starts it as a
 * worker (/js/soundtrack.js) the first time the intro's sound button is pressed, so a visitor who never asks for
 * sound never pays for it, and the fire's sample-by-sample synthesis (client/lib/fire-sound.ts) never stalls the
 * picture.
 *
 * It answers one message, `Ask`, with two: the soundtrack first, since the player is waiting for it, then the
 * campfire loop, which is not needed until a few seconds in. Their samples are transferred, not copied.
 */

import { type Ask, type Made, campfire, soundtrack } from './lib/intro-soundtrack';

type Scope = {
	onmessage: ((e: MessageEvent<Ask>) => void) | null;
	postMessage(message: Made, transfer: Transferable[]): void;
};
const scope = self as unknown as Scope;

scope.onmessage = ({ data }) => {
	const track = soundtrack(data.sr, data.seed);
	scope.postMessage({ kind: 'track', l: track.l, r: track.r }, [track.l.buffer, track.r.buffer]);
	const fire = campfire(data.sr, data.seed + 1, track.gain);
	scope.postMessage({ kind: 'campfire', l: fire.l, r: fire.r }, [fire.l.buffer, fire.r.buffer]);
};
