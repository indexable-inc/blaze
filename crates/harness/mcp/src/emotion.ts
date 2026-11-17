// Agent-reported work mood is ephemeral native UI state, sent through the existing versioned app socket.
import type { Docs } from './docs.ts';
import { session } from './self.ts';
import { SOCK } from './daemon.ts';
import { ask } from './world.ts';

export const EMOTIONS = ['focused', 'curious', 'confident', 'uncertain', 'frustrated', 'blocked', 'relieved'] as const;
export type Emotion = typeof EMOTIONS[number];
export type EmotionUpdate = { session: number; emotion: Emotion; emoji: string; changed: boolean };

export function emotionPlan(emotion: Emotion, reason = ''): { emotion: Emotion; reason: string } {
  if (!EMOTIONS.includes(emotion)) throw new TypeError(`updateEmotion: emotion must be ${EMOTIONS.join(', ')}`);
  if (typeof reason !== 'string' || [...reason].length > 240 || /\p{Cc}/u.test(reason)) {
    throw new TypeError('updateEmotion: reason must be one line of at most 240 characters');
  }
  return { emotion, reason: reason.trim() };
}

/** Report the calling agent's current work mood; never changes another session and never persists a mood across launches. */
export async function updateEmotion(emotion: Emotion, reason = ''): Promise<EmotionUpdate> {
  const plan = emotionPlan(emotion, reason);
  const sid = session();
  if (sid === null || !Number.isSafeInteger(sid) || sid < 1) throw new Error('updateEmotion requires a valid Harness agent session (HARNESS_SESSION)');
  const result = await ask<unknown>({ op: 'emotion', device: SOCK(), session: sid, ...plan });
  if (typeof result !== 'object' || result === null || !('session' in result) || result.session !== sid
    || !('emotion' in result) || result.emotion !== emotion || !('emoji' in result) || typeof result.emoji !== 'string'
    || !('changed' in result) || typeof result.changed !== 'boolean') {
    throw new Error(`updateEmotion: the Harness window returned an invalid result for s${sid}`);
  }
  return { session: sid, emotion, emoji: result.emoji, changed: result.changed };
}

export const docs: Docs = {
  updateEmotion: {
    area: 'agents', for: 'report work mood confidence uncertainty frustration blocker relief in the sidebar',
    sig: 'await updateEmotion(emotion, reason?) -> {session, emotion, emoji, changed}',
    about: 'Report your current work mood: focused 🎯, curious 🧐, confident 🙂, uncertain 🤔, frustrated 😖, blocked 🚧, relieved 😌. The optional reason is one line of at most 240 characters. Your own agent row shows the emoji with the reason in a tooltip. Entering uncertain, frustrated or blocked gives one soft sound by default; repeated identical moods stay quiet. This is live work-state reporting, not a claim of human feelings. Requires the native Harness window; changes are ephemeral and never replayed or journaled.',
    example: "await updateEmotion('uncertain', 'The stated speed metric may not measure the edit loop you care about')",
    throws: 'TypeError for an unknown mood or invalid reason; Error without a Harness session, if its row is absent, or if the native app is unavailable.',
  },
};
