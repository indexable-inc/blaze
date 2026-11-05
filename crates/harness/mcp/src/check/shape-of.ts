import { ElementFlags, SymbolFlags, TypeFlags, isIntrinsicType, isLiteralType, isIntersectionType, isTupleType, isTypeReference, isUnionType } from 'typescript/unstable/async';
import type { Checker, Type } from 'typescript/unstable/async';
import { tagOf } from './shape.ts';
import type { Plan, Shape } from './shape.ts';

/*
 * A TypeScript type as a shape (shape.ts), read from TypeScript's own checker at check time: what `parse<T>(value)`
 * checks a value against at run time, compiled from `T` as typia does, with no second schema to keep in step.
 */

/** Why a type has no shape: what it is, and where in the type it sits. */
export class Unsupported extends Error {
  constructor(readonly type: string, readonly at: string, readonly why: string) { super(`${type}${at ? ` at ${at}` : ''}: ${why}`); }
}

const membersOf = async (type: Type): Promise<readonly Type[]> => isUnionType(type) || isIntersectionType(type) ? await type.getTypes() : [];
const PRIMITIVE = TypeFlags.String | TypeFlags.Number | TypeFlags.Boolean | TypeFlags.StringLiteral | TypeFlags.NumberLiteral | TypeFlags.BooleanLiteral | TypeFlags.Null | TypeFlags.Undefined;

/** `type` as a plan, named `name`; throws `Unsupported` for a part no value can be checked against. */
export async function shapeOf(checker: Checker, type: Type, name: string): Promise<Plan> {
  const defs: Record<string, Shape> = {};
  /** Object types being read, so one that contains itself becomes a `ref`. */
  const active = new Map<number, { id: string; recursive: boolean }>();
  const text = (part: Type) => checker.typeToString(part);
  const isFunctionLike = async (part: Type) => (await checker.getSignaturesOfType(part, 0)).length > 0 || (await checker.getSignaturesOfType(part, 1)).length > 0;

  const walk = async (part: Type, at: string): Promise<Shape> => {
    const flags = part.flags;
    const unsupported = async (why: string) => { throw new Unsupported(await text(part), at, why); };
    if (flags & TypeFlags.Any) return unsupported('`any` says nothing to check; write the type it is');
    if (flags & TypeFlags.Unknown) return { t: 'unknown' };
    if (flags & TypeFlags.Never) return { t: 'union', of: [] };
    if (flags & TypeFlags.BooleanLiteral) return { t: 'literal', value: isIntrinsicType(part) && part.intrinsicName === 'true' };
    if (flags & (TypeFlags.StringLiteral | TypeFlags.NumberLiteral) && isLiteralType(part) && (typeof part.value === 'string' || typeof part.value === 'number')) return { t: 'literal', value: part.value };
    if (flags & TypeFlags.String) return { t: 'string' };
    if (flags & TypeFlags.Number) return { t: 'number' };
    if (flags & TypeFlags.Boolean) return { t: 'boolean' };
    if (flags & TypeFlags.Null) return { t: 'null' };
    if (flags & (TypeFlags.Undefined | TypeFlags.Void)) return { t: 'undefined' };
    if (flags & (TypeFlags.BigInt | TypeFlags.BigIntLiteral)) return unsupported('a bigint is not JSON; take a string or a number and convert it');
    if (flags & (TypeFlags.ESSymbol | TypeFlags.UniqueESSymbol)) return unsupported('a symbol is not data');
    if (flags & TypeFlags.TypeParameter) return unsupported('a type parameter has no shape until it is a type; parse where the type is known');
    if (flags & (TypeFlags.TemplateLiteral | TypeFlags.StringMapping)) return unsupported('a template literal type is not checked; use `string`, or a union of the strings');
    if (flags & TypeFlags.Union) {
      const members = await Promise.all((await membersOf(part)).map(member => walk(member, at)));
      const isTrue = (m: Shape) => m.t === 'literal' && m.value === true;
      const isFalse = (m: Shape) => m.t === 'literal' && m.value === false;
      const merged = members.some(isTrue) && members.some(isFalse) ? [...members.filter(m => !isTrue(m) && !isFalse(m)), { t: 'boolean' } as Shape] : members;
      if (merged.length === 1) return merged[0]!;
      const tag = tagOf(merged, defs);
      return tag === undefined ? { t: 'union', of: merged } : { t: 'union', of: merged, tag };
    }
    if (flags & TypeFlags.Intersection) {
      // A primitive with an object beside it is a brand (`string & { __brand: 'Id' }`): only the primitive exists at run time.
      const members = await membersOf(part);
      const primitives = members.filter(member => member.flags & PRIMITIVE);
      if (primitives.length === 1) return walk(primitives[0]!, at);
      if (primitives.length > 1) return unsupported('no value is all of these at once');
      return objectShape(part, at);
    }
    if (flags & TypeFlags.NonPrimitive) return unsupported('`object` says nothing about the fields; write them, or use `Record<string, unknown>`');
    if (flags & TypeFlags.Object) return objectShape(part, at);
    return unsupported('this kind of type is not checked at run time');
  };

  const objectShape = async (part: Type, at: string): Promise<Shape> => {
    const seen = active.get(part.id);
    if (seen) { seen.recursive = true; return { t: 'ref', id: seen.id }; }
    if (isTypeReference(part) && await checker.isArrayType(part)) {
      const [element] = await checker.getTypeArguments(part);
      return { t: 'array', of: element ? await walk(element, `${at}[]`) : { t: 'unknown' } };
    }
    if (isTypeReference(part) && await checker.isTupleType(part)) {
      const target = await part.getTarget();
      const elementFlags = isTupleType(target) ? target.elementFlags : [];
      const elements = (await checker.getTypeArguments(part)).slice(0, elementFlags.length);
      const of: Shape[] = [];
      let optional = 0;
      let rest: Shape | undefined;
      for (const [index, element] of elements.entries()) {
        const flag = elementFlags[index] ?? ElementFlags.Required;
        const shape = await walk(element, `${at}[${index}]`);
        if (flag & ElementFlags.Variable) { if (rest || index !== elements.length - 1) throw new Unsupported(await text(part), at, 'a tuple with a spread in its middle is not checked'); rest = shape; }
        else { of.push(shape); if (flag & ElementFlags.Optional) optional++; }
      }
      return { t: 'tuple', of, optional, ...(rest ? { rest } : {}) };
    }
    const symbol = await part.getSymbol();
    if (symbol?.name === 'Date' && (await checker.getPropertyOfType(part, 'toISOString'))) return { t: 'date' };
    if (await isFunctionLike(part)) throw new Unsupported(await text(part), at, 'a function is not data; parse the data, then build the rest from it');
    const id = `d${part.id}`;
    const frame = { id, recursive: false };
    active.set(part.id, frame);
    try {
      const properties = await checker.getPropertiesOfType(part);
      const types = properties.length ? await checker.getTypeOfSymbol(properties) : [];
      const fields: Record<string, Shape> = {};
      const optional: string[] = [];
      for (const [index, property] of properties.entries()) {
        const where = `${at ? `${at}.` : ''}${property.name}`;
        if (property.flags & SymbolFlags.Method) throw new Unsupported(await text(part), where, `a method is not data: ${await text(part)} is a class or an API, not a value parse can make; parse the data it is made from`);
        const propertyType = types[index];
        if (!propertyType) throw new Unsupported(await text(part), where, 'its type could not be read');
        if (!(propertyType.flags & TypeFlags.Union) && (propertyType.flags & TypeFlags.Object) && await isFunctionLike(propertyType)) throw new Unsupported(await text(propertyType), where, 'a function is not data; parse the data, then build the rest from it');
        fields[property.name] = await walk(propertyType, where);
        if (property.flags & SymbolFlags.Optional) optional.push(property.name);
      }
      let rest: Shape | undefined;
      for (const info of await checker.getIndexInfosOfType(part)) {
        if (info.keyType.flags & (TypeFlags.ESSymbol | TypeFlags.UniqueESSymbol)) continue;
        const value = await walk(info.valueType, `${at}[key]`);
        rest = rest ? { t: 'union', of: [rest, value] } : value;
      }
      const shape: Shape = { t: 'object', fields, optional, ...(rest ? { rest } : {}) };
      if (!frame.recursive) return shape;
      defs[id] = shape;
      return { t: 'ref', id };
    } finally { active.delete(part.id); }
  };

  return { name, root: await walk(type, ''), defs };
}
