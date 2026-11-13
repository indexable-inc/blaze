/**
 * The intro's timeline, in seconds from when it starts: the one place its moments are written. The picture
 * (client/lib/wordmark.ts) and the soundtrack (client/lib/intro-soundtrack.ts) both read these, so a letter's landing
 * and its note can't drift apart. The values are anim's (src/blaze.rs in indexable-inc/anim), which are the brand
 * fire's (docs/brand/fire/gen.py).
 *
 * Under client/lib/ (not bundled as an entry).
 */

/** Fire steps a second. */
export const FPS = 16;
/** When each letter (b l a z e) starts to fall, and how long the fall takes. */
export const DROP_START = [0.15, 0.31, 0.47, 0.63, 0.79] as const;
export const DROP_DUR = 0.36;
/** The last letter is down. */
export const LANDED = DROP_START[4] + DROP_DUR;
/** The fire catches a beat after it. */
export const IGNITE_AT = LANDED + 0.2;
/** Seconds for the fire to catch across the whole word, from the b to the e. */
export const IGNITE = 0.75;
/** The word is alight from end to end: the rest of the page may show. */
export const LIT = IGNITE_AT + IGNITE;
/** The soundtrack's own fire dies down as a small campfire, which loops for as long as the sound is on, fades in. */
export const CAMPFIRE_AT = IGNITE_AT + 3;
