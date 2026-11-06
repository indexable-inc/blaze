// Ported from den (repl/src/pmap.ts) unchanged.
/**
 * `items.map(fn)` run `concurrency` at a time, results in input order.
 *
 * A `for (...) await` loop over independent items waits for each one before
 * starting the next; measured 2026-09-24 on 17,235 files (read and search each),
 * that loop took 2.1 s and this took 1.1 to 1.4 s, and for network calls the
 * gap grows with the concurrency. It is bounded because unbounded fan-out
 * trips rate limits and open-file limits. The first error aborts `signal`,
 * stops starting new items, waits for the ones in flight, then is thrown.
 */
export async function pmap<T, R>(
  items: Iterable<T> | AsyncIterable<T>,
  fn: (item: T, index: number, signal: AbortSignal) => R | Promise<R>,
  options: { concurrency?: number } | number = {},
): Promise<R[]> {
  if (typeof options !== 'number' && (typeof options !== 'object' || options === null)) throw new TypeError('pmap(items, fn, { concurrency }) takes an options object or a number');
  const concurrency = typeof options === 'number' ? options : options.concurrency ?? 16;
  if (!(concurrency >= 1) || (!Number.isInteger(concurrency) && concurrency !== Infinity)) {
    throw new RangeError(`pmap concurrency must be a positive integer or Infinity, got ${concurrency}`);
  }
  if (typeof fn !== 'function') throw new TypeError('pmap(items, fn, { concurrency }) needs a function');
  const asynchronous = Symbol.asyncIterator in Object(items);
  // Unbounded needs a known length; a sync iterable's elements pass through as they are, as they do when bounded.
  if (concurrency === Infinity && !Array.isArray(items)) items = asynchronous ? await Array.fromAsync(items as AsyncIterable<T>) : Array.from(items as Iterable<T>);
  const iterator = Symbol.asyncIterator in Object(items)
    ? (items as AsyncIterable<T>)[Symbol.asyncIterator]()
    : (items as Iterable<T>)[Symbol.iterator]();
  const controller = new AbortController();
  const results: R[] = [];
  let next = 0;
  let failure: { error: unknown } | undefined;
  const fail = (error: unknown) => {
    failure ??= { error };
    controller.abort(error);
  };
  const worker = async () => {
    while (!failure) {
      let step: IteratorResult<T>;
      // An iterator that throws is a failure like any other: abort, then wait for the items in flight.
      try { step = await iterator.next(); }
      catch (error) { fail(error); return; }
      if (step.done || failure) return;
      const index = next++;
      try { results[index] = await fn(step.value, index, controller.signal); }
      catch (error) { fail(error); }
    }
  };
  const workers = Array.isArray(items) ? Math.min(concurrency, items.length) : concurrency;
  await Promise.all(Array.from({ length: workers }, worker));
  if (failure) {
    // Closing the iterator must not replace the error that stopped the run.
    try { await (iterator as { return?: () => unknown }).return?.(); } catch { /* the first error wins */ }
    throw failure.error;
  }
  return results;
}
