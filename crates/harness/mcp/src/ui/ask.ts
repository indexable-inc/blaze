import { MAX_LABEL, MAX_OPTIONS } from './element.ts';

/*
 * `ask(text, options)`: put a question to the person watching, in the calling cell's own UI. It is drawn with the same elements
 * (a Card holding a Choice or Select of the options, and an Input when the person may type) and its answer comes back the same way
 * every control's does (the app's ui_event, index.ts `dispatch`): there is no other channel. Copied from den's repl/src/ask.ts, which
 * asks through den's hub instead; the rules are den's but one: here a question always waits. The cell waits for the
 * answer, however long it takes, until the cell stops with an error (its timeout), which withdraws the question. A `default` is
 * the option recommended: it shows picked, with an OK that gives it; it is never taken for the person. Only `block: false` (with
 * a default) goes ahead: it returns the default at once, the cell carries on, and the question stays up; if the person later
 * answers something else, the cell's logs say so. The question is one line (at most ASK_MAX characters): what it rests on goes in
 * `detail`, or is shown above it. Where it goes: below whatever the cell already shows, and it stays there answered.
 */

export interface AskOption { label: string; detail?: string }
export interface AskOptions {
  /** The choices: labels, or `{label, detail}`. Without any the question is free text. */
  options?: readonly (string | AskOption)[];
  /** The option you recommend, shown picked: its label, or its index. Never taken for the person unless `block: false`. */
  default?: string | number;
  /** One line of context under the question. */
  detail?: string;
  /** Let the person type an answer too (always on when there are no options). */
  allowText?: boolean;
  /** Wait for the answer (the default). `false` with a `default`: go ahead with it at once, for a choice that is cheap to change. */
  block?: boolean;
}

/** A question, checked: `default` is an index into `options`. */
export interface AskPlan {
  text: string;
  detail?: string;
  options: AskOption[];
  default?: number;
  allowText: boolean;
  block: boolean;
}

/** The most characters a question runs to: one line a person reads at a glance. */
export const ASK_MAX = 140;

/** The question for `text` and `options`, checked: every mistake is the caller's, so each throws before anything is shown. */
export function planAsk(text: string, options: AskOptions = {}): AskPlan {
  if (typeof text !== 'string' || !text.trim()) throw new TypeError('ask(text, options) needs the question as a non-empty string');
  if (text.length > ASK_MAX) throw new RangeError(`ask: the question is ${text.length} characters; at most ${ASK_MAX}. Ask only the question ("Stop them?"); what it rests on goes in detail, or in a Table or Stat shown above it`);
  if (typeof options !== 'object' || options === null || Array.isArray(options)) throw new TypeError('ask(text, options) takes an options object');
  const stray = Object.keys(options).find(key => !['options', 'default', 'detail', 'allowText', 'block'].includes(key));
  if (stray) throw new TypeError(`ask: no option ${JSON.stringify(stray)}; it takes options, default, detail, allowText, block`);
  const given = options.options ?? [];
  if (!Array.isArray(given)) throw new TypeError('ask: options takes an array of labels or {label, detail}');
  if (given.length > MAX_OPTIONS) throw new RangeError(`ask: ${given.length} options; at most ${MAX_OPTIONS}`);
  const choices = given.map((option, index): AskOption => {
    const choice = typeof option === 'string' ? { label: option } : option;
    if (typeof choice?.label !== 'string' || !choice.label.trim() || choice.label.length > MAX_LABEL) throw new TypeError(`ask: option ${index} needs a label of 1 to ${MAX_LABEL} characters`);
    if (choice.detail !== undefined && typeof choice.detail !== 'string') throw new TypeError(`ask: option ${index} has a detail that is not a string`);
    return choice.detail === undefined ? { label: choice.label } : { label: choice.label, detail: choice.detail };
  });
  if (new Set(choices.map(choice => choice.label)).size !== choices.length) throw new TypeError('ask: two options have the same label');
  let preset: number | undefined;
  if (options.default !== undefined) {
    preset = typeof options.default === 'number' ? options.default : choices.findIndex(choice => choice.label === options.default);
    if (!Number.isInteger(preset) || preset < 0 || preset >= choices.length) {
      throw new RangeError(`ask: default ${JSON.stringify(options.default)} is not one of the options (${choices.map(choice => JSON.stringify(choice.label)).join(', ') || 'none'})`);
    }
  }
  const block = options.block ?? true;
  if (typeof block !== 'boolean') throw new TypeError('ask: block takes true or false');
  if (!block && preset === undefined) throw new TypeError('ask: a question that does not wait needs a default to go ahead with');
  if (options.detail !== undefined && typeof options.detail !== 'string') throw new TypeError('ask: detail takes a string');
  if (options.allowText !== undefined && typeof options.allowText !== 'boolean') throw new TypeError('ask: allowText takes true or false');
  return {
    text, ...(options.detail === undefined ? {} : { detail: options.detail }),
    options: choices, ...(preset === undefined ? {} : { default: preset }),
    allowText: options.allowText ?? choices.length === 0, block,
  };
}

/** The most options shown side by side as a Choice; more drop from a Select. */
export const CHOICE_MAX = 6;

/**
 * The question as a component: its words as the Card's title, its detail, each option's detail, then a Choice (or a Select) of the
 * options and, when the person may type, an Input with a Send button. A blocking question with a default shows it picked and an OK
 * that gives it. Once answered it shows the answer and takes nothing more. `answer` hears the label picked or the words typed.
 */
export const ASK_SOURCE = `<script>
  let { text, detail, options, details, preset, ahead, allowText, answer } = $props();
  let picked = $state(preset ?? null);
  let typed = $state('');
  let said = $state(undefined);
  const give = (value) => {
    const words = String(value ?? '').trim();
    if (!words || said !== undefined) return;
    said = words;
    answer(words);
  };
</script>
<Card title={text} gap={8}>
  {#if detail}<Text tone="muted">{detail}</Text>{/if}
  {#if said !== undefined}
    <Row gap={6}><Text tone="muted">Answered</Text><Badge tone="accent">{said}</Badge></Row>
  {:else}
    {#each details as line}<Text tone="muted">{line}</Text>{/each}
    {#if ahead}<Text tone="muted">Went ahead with {ahead}; pick another to change it.</Text>{/if}
    {#if options.length > ${CHOICE_MAX}}
      <Select options={options} bind:selected={picked} placeholder="Pick one" onchange={give} />
    {:else if options.length > 0}
      <Choice options={options} bind:selected={picked} onchange={give} />
    {/if}
    {#if preset && !ahead}<Button tone="accent" onclick={() => give(picked)}>OK</Button>{/if}
    {#if allowText}
      <Row gap={6}>
        <Input bind:value={typed} placeholder="Type an answer" onsubmit={give} />
        <Button tone="accent" disabled={!typed.trim()} onclick={() => give(typed)}>Send</Button>
      </Row>
    {/if}
  {/if}
</Card>`;

/** The props the question's component is mounted with, besides the elements and `answer`. */
export function askProps(plan: AskPlan): Record<string, unknown> {
  const preset = plan.default === undefined ? undefined : plan.options[plan.default]!.label;
  return {
    text: plan.text, detail: plan.detail,
    options: plan.options.map(option => option.label),
    details: plan.options.filter(option => option.detail).map(option => `${option.label}: ${option.detail}`),
    preset, ahead: plan.block ? undefined : preset, allowText: plan.allowText,
  };
}
