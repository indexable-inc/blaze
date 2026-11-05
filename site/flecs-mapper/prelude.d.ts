// Shared declarations for the TypeScript flecs-content-mapper generates from .flecs files. Every
// generated file references this one; the files add to the interfaces and to namespace __e.

/** Every named entity: `__e.site.charter` is the entity site.charter. */
declare namespace __e {}

/** Each struct component's named form, by full path: `{ text?: string }` for `struct P(text: string)`. */
interface __Structs {}
/** Each struct component's positional form: `[text?: string]`. */
interface __Tuples {}
/** Each prefab's markers, its bases' included: `Rfc` has `__isa_Rfc`, `__isa_Doc`, `__isa_Page`. */
interface __Prefabs {}

type __Named<K extends keyof __Structs> = __Structs[K];
type __Positional<K extends keyof __Tuples> = __Tuples[K];
/** A member whose type is another struct takes either form. */
type __Val<K extends string> = K extends keyof __Structs ? __Structs[K] | __Tuples[K & keyof __Tuples] : never;
type __PrefabOf<K extends string> = K extends keyof __Prefabs ? __Prefabs[K] : { readonly [P in `__isa_${K}`]: 0 };
/** What the target of a relationship with a rule must be: an instance of prefab K, or have K. */
type __Target<K extends string> = K extends keyof __Prefabs
	? { readonly __isa: { readonly [P in `__isa_${K}`]: 0 } }
	: { readonly [P in `__has_${K}`]: 0 };
/** Checks the target of a relationship with a rule; an error lands on the target. */
declare function __target<K extends string>(target: __Target<K>): void;
/** A member of type flecs.meta.entity. */
type __Entity = { readonly __: 0 };
