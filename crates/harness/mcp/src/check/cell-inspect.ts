import { TypeFlags } from 'typescript/unstable/async';
import type { Checker, Program, Type } from 'typescript/unstable/async';
import { SyntaxKind, getTokenPosOfNode } from 'typescript/unstable/ast';
import type { Node, SourceFile } from 'typescript/unstable/ast';
import { shapeOf, Unsupported } from './shape-of.ts';
import type { CellDiagnostic, Inspection, ParseSite } from './cell-types.ts';

/*
 * What a cell's check reads from TypeScript's checker beyond its diagnostics (type-service.ts): each `parse<T>(value)`
 * compiled to the shape it checks at run time, and each `as T` on an `unknown` value, which is a guess the checker
 * cannot see and so is refused.
 */

/** den's own diagnostics, numbered past TypeScript's (cell-types.ts gives their hints). */
const PARSE_UNSUPPORTED = 90001;
const PARSE_UNTYPED = 90002;
const UNKNOWN_CAST = 90003;

type AnyNode = Node & { expression?: Node; typeArguments?: readonly Node[]; arguments?: readonly Node[]; type?: Node; text?: string };

const unwrapped = (node: AnyNode): AnyNode => {
  let inner = node;
  while (inner.kind === SyntaxKind.ParenthesizedExpression && inner.expression) inner = inner.expression as AnyNode;
  return inner;
};
/** `x as unknown`: the first half of the `as unknown as T` escape, allowed (with a reason) by cell-types.ts. */
const toUnknown = (node: AnyNode) => (node.kind === SyntaxKind.AsExpression || node.kind === SyntaxKind.TypeAssertionExpression) && node.type?.kind === SyntaxKind.UnknownKeyword;

/**
 * Reads `file` (the lane's `cell.ts`, whose cell starts `offset` characters in, after the header) for parse calls and
 * casts of unknown values. `prelude` is the file declaring the globals, so only the global `parse` is compiled.
 */
export async function inspectCell(program: Program, checker: Checker, file: string, offset: number, prelude: string, lineOf: (at: number) => number): Promise<Inspection> {
  const diagnostics: CellDiagnostic[] = [];
  const parses: ParseSite[] = [];
  const source = await program.getSourceFile(file) as SourceFile | undefined;
  if (!source) return { diagnostics, parses };
  const calls: AnyNode[] = [];
  const casts: AnyNode[] = [];
  const visit = (node: Node): void => {
    const n = node as AnyNode;
    if (n.kind === SyntaxKind.CallExpression && n.expression?.kind === SyntaxKind.Identifier && (n.expression as AnyNode).text === 'parse') calls.push(n);
    if (n.kind === SyntaxKind.AsExpression || n.kind === SyntaxKind.TypeAssertionExpression) casts.push(n);
    node.forEachChild(visit);
  };
  source.forEachChild(visit);
  const start = (node: Node) => getTokenPosOfNode(node, source) - offset;
  const line = (node: Node) => lineOf(Math.max(0, start(node)));

  for (const call of calls) {
    const symbol = await checker.getSymbolAtLocation(call.expression!);
    if (!symbol?.declarations.some(declaration => declaration.path.endsWith(prelude))) continue;
    const argument = call.typeArguments?.[0];
    if (!argument) {
      diagnostics.push({ line: line(call), code: PARSE_UNTYPED, text: '`parse` needs the type to check against: `parse<{ id: number }>(value)`.' });
      continue;
    }
    const type = await checker.getTypeFromTypeNode(argument as never) as Type | undefined;
    const name = source.text.slice(getTokenPosOfNode(argument, source), argument.end).trim();
    if (!type) continue;
    try {
      const plan = await shapeOf(checker, type, name);
      const last = call.arguments?.at(-1);
      parses.push({ at: (last ? last.end : call.end - 1) - offset, plan });
    } catch (error) {
      if (!(error instanceof Unsupported)) throw error;
      diagnostics.push({ line: line(argument), code: PARSE_UNSUPPORTED, text: `parse<${name}> cannot check ${error.at ? `\`${error.at}\`, ` : ''}\`${error.type}\` at run time: ${error.why}.` });
    }
  }

  for (const cast of casts) {
    if (cast.type?.kind === SyntaxKind.UnknownKeyword || cast.type?.kind === SyntaxKind.ConstKeyword) continue;
    const inner = unwrapped(cast.expression as AnyNode);
    if (toUnknown(inner)) continue;
    const type = await checker.getTypeAtLocation(inner) as Type | undefined;
    if (!type || !(type.flags & TypeFlags.Unknown)) continue;
    const target = source.text.slice(getTokenPosOfNode(cast.type!, source), cast.type!.end).trim();
    diagnostics.push({ line: line(cast), code: UNKNOWN_CAST, text: `\`as ${target}\` on an \`unknown\` value is a guess nothing checks; \`parse<${target}>(value)\` checks it and says what does not fit.` });
  }
  return { diagnostics, parses };
}
