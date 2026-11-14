import { expect, test } from 'bun:test';
import { planAsk } from './ask.ts';

// A recommendation is not an answer: material missing priorities must keep waiting for the person.
test('ask waits even with a recommended default; going ahead requires an explicit valid default', () => {
  const options = ['Latency', 'Reliability'];
  expect(planAsk('Which priority matters most?', { options, default: 'Reliability' }))
    .toMatchObject({ block: true, default: 1, allowText: false });
  expect(planAsk('Which priority matters most?'))
    .toMatchObject({ block: true, options: [], allowText: true });
  expect(() => planAsk('Which priority matters most?', { options, block: false }))
    .toThrow('a question that does not wait needs a default');
  expect(planAsk('Which priority matters most?', { options, default: 0, block: false, allowText: true }))
    .toMatchObject({ block: false, default: 0, allowText: true });
});
