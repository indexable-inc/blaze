import { expect, test } from 'bun:test';
import { emotionPlan, EMOTIONS, type Emotion } from './emotion.ts';

test('work moods and a brief reason are checked before contacting the native app', () => {
  for (const mood of EMOTIONS) expect(emotionPlan(mood, '  Checking the evidence  ')).toEqual({ emotion: mood, reason: 'Checking the evidence' });
  expect(() => emotionPlan('invented' as Emotion)).toThrow('emotion must be');
  for (const reason of ['two\nlines', 'control\u0085character', 'x'.repeat(241)]) {
    expect(() => emotionPlan('focused', reason)).toThrow('one line of at most 240 characters');
  }
  expect(emotionPlan('curious', '🧐'.repeat(240)).reason).toHaveLength(480);
});
