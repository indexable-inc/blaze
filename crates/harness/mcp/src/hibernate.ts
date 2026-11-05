import { deserialize, serialize } from 'node:v8';

/** Largest single binding kept (serialized): a bigger one is dropped, named in the note, rather than slowing every recycle. */
const MAX_BYTES = 32 * 1024 * 1024;
/** Most values visited in one binding, so a huge or cyclic-looking structure cannot hold the evaluator. */
const MAX_NODES = 500_000;
const TYPED = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array, Float32Array, Float64Array, BigInt64Array, BigUint64Array];

/**
 * Whether a value survives a save and restore as itself: primitives, arrays, plain objects, Map, Set, Date, RegExp,
 * ArrayBuffer and typed arrays, all the way down. structuredClone alone is not the test: it turns a class instance
 * into a plain object that has lost its prototype and methods, which would restore a lie. Functions, symbols,
 * promises, errors, class instances, accessors and anything holding a handle (a Playwright page, a socket, a child
 * process) are all refused.
 */
export function cloneable(root: unknown): boolean {
  const seen = new Set<unknown>();
  let nodes = 0;
  const walk = (value: unknown): boolean => {
    if (value === null || ['undefined', 'boolean', 'number', 'string', 'bigint'].includes(typeof value)) return true;
    if (typeof value !== 'object') return false;
    if (seen.has(value)) return true;
    seen.add(value);
    if (++nodes > MAX_NODES) return false;
    const prototype = Object.getPrototypeOf(value);
    if (Object.getOwnPropertySymbols(value).length) return false;
    if (value instanceof Date) return prototype === Date.prototype;
    if (value instanceof RegExp) return prototype === RegExp.prototype;
    if (value instanceof ArrayBuffer) return prototype === ArrayBuffer.prototype;
    if (ArrayBuffer.isView(value)) return TYPED.some(type => prototype === type.prototype);
    if (value instanceof Map) return prototype === Map.prototype && [...value].every(([key, item]) => walk(key) && walk(item));
    if (value instanceof Set) return prototype === Set.prototype && [...value].every(walk);
    if (Array.isArray(value)) return prototype === Array.prototype && Object.getOwnPropertyNames(value).every(key => key === 'length' || walkProperty(value, key));
    if (prototype !== Object.prototype && prototype !== null) return false;
    return Object.getOwnPropertyNames(value).every(key => walkProperty(value, key));
  };
  const walkProperty = (owner: object, key: string): boolean => {
    const descriptor = Object.getOwnPropertyDescriptor(owner, key)!;
    return 'value' in descriptor && walk(descriptor.value);
  };
  return walk(root);
}

/** The bindings that can be saved (each within the size cap) and the names of those that cannot. */
export function snapshot(bindings: Iterable<[string, () => unknown]>): { saved: Map<string, unknown>; dropped: string[] } {
  const saved = new Map<string, unknown>();
  const dropped: string[] = [];
  for (const [name, get] of bindings) {
    try {
      const value = get();
      if (cloneable(value) && serialize(value).length <= MAX_BYTES) saved.set(name, value);
      else dropped.push(name);
    } catch { dropped.push(name); }
  }
  return { saved, dropped };
}

/** All saved bindings in one serialization, so two names that held one object, and cycles through several, come back as one. */
export const encode = (saved: Map<string, unknown>): Buffer => serialize(saved);
export const decode = (bytes: Buffer): Map<string, unknown> => deserialize(bytes) as Map<string, unknown>;
