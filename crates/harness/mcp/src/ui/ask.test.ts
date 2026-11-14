import { expect, test } from 'bun:test';
import { planAsk } from './ask.ts';
import { elementWire, questionOptionsOf } from './element.ts';
import { Kernel } from '../kernel.ts';
import { getTask } from '../tasks.ts';
import { dispatch } from './index.ts';
import { RENDER_MS } from './tree.ts';

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

test('a question keeps option explanations and an explicit recommendation without submitting it', async () => {
  const kernel = new Kernel({});
  const pending = await kernel.handle({ op: 'run', code: `await ask('Which outcome matters most?', {
    options: [{label:'Latency', detail:'Faster feedback while editing'}, {label:'Reliability', detail:'Fewer interrupted tasks'}],
    default:'Reliability', allowText:true
  })`, yield_ms: 50 });
  const task = getTask(pending.task!);
  try {
    expect(pending.status).toBe('running');
    expect(task.ui).toMatchObject({ type: 'Question', props: {
      text: 'Which outcome matters most?', recommended: 'Reliability', allowText: true, value: '',
      options: [{label:'Latency', detail:'Faster feedback while editing'}, {label:'Reliability', detail:'Fewer interrupted tasks'}],
    } });
    const props = (task.ui as {props: Record<string, string>}).props;
    expect(props.answer).toBeUndefined();
    await dispatch(task.id, props.onChoose!, 'Latency');
    await task.done;
    await Bun.sleep(RENDER_MS * 2); // The real wire emitter coalesces a frame's UI changes.
    expect(task.value).toBe('Latency');
    expect(task.ui).toMatchObject({ type: 'Question', props: {answer:'Latency'} });
    await dispatch(task.id, props.onSubmit!, 'Reliability');
    expect(task.value).toBe('Latency');
  } finally {
    if (task.status === 'running') await kernel.handle({op:'cancel', task:task.id});
  }
});

test('a free-text question submits the exact response and becomes an answered receipt', async () => {
  const kernel = new Kernel({});
  const pending = await kernel.handle({op:'run', code:`await ask('What are you building?')`, yield_ms:50});
  const task = getTask(pending.task!);
  try {
    expect(pending.status).toBe('running');
    const props = (task.ui as {props: Record<string, string>}).props;
    await dispatch(task.id, props.onChange!, 'A native interface');
    await Bun.sleep(RENDER_MS * 2);
    expect(task.ui).toMatchObject({props:{value:'A native interface'}});
    await dispatch(task.id, props.onSubmit!, 'A native interface');
    await task.done;
    await Bun.sleep(RENDER_MS * 2);
    expect(task.value).toBe('A native interface');
    expect(task.ui).toMatchObject({type:'Question', props:{answer:'A native interface'}});
  } finally {
    if (task.status === 'running') await kernel.handle({op:'cancel', task:task.id});
  }
});

test('question wire data refuses unanswerable or ambiguous choices instead of rendering dead controls', () => {
  expect(questionOptionsOf([{label:'One', detail:'Explanation'}])).toEqual([{label:'One', detail:'Explanation'}]);
  expect(questionOptionsOf([{label:'One'}, {label:'One'}])).toContain('unique');
  expect(questionOptionsOf([{label:'One', extra:true}])).toContain('unknown field');
  expect(() => elementWire('Question', {text:'Choose?', options:[{label:'One'}], recommended:'Missing'})).toThrow('recommended');
  expect(() => planAsk('Choose?', {allowText:false})).toThrow('options or a free-text answer');
  expect(() => elementWire('Question', {text:'Choose?', options:[], allowText:false})).toThrow('no way to answer');
});
