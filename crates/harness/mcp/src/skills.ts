// Skills: the instructions for one kind of work, loaded when a task needs them. The daemon finds them
// (harness/src/skills.rs: this kernel's own skills/ folder, $HARNESS_HOME/skills, the project's .agents/skills and
// .claude/skills), puts their index in the agent's system note, and hands this kernel the same list as HARNESS_SKILLS
// (JSON [{ name, description, source, path }]), so the index the agent reads and what skill() serves are one list.
// skill(name) reads the file when asked (an edit shows at the next read) and returns its body, the front matter cut;
// the person sees it as one quiet step, "Read skill github".
import { dirname } from "node:path";
import { bytes, effect } from "./effects.ts";
import { table, type Table } from "./table.ts";

export type SkillSource = "harness" | "user" | "project";
export type SkillRow = { name: string; description: string; source: SkillSource; path: string };

const isRow = (v: unknown): v is SkillRow => {
  if (typeof v !== "object" || v === null) return false;
  const r = v as Record<string, unknown>;
  return typeof r.name === "string" && typeof r.description === "string" && typeof r.path === "string"
    && (r.source === "harness" || r.source === "user" || r.source === "project");
};

let known: SkillRow[] | undefined;
function list(): SkillRow[] {
  if (known) return known;
  const raw = process.env.HARNESS_SKILLS;
  if (raw === undefined) {
    throw new Error("skills: HARNESS_SKILLS is unset; the harness daemon sets it when it launches an agent (harness/src/profiles.rs), so this kernel was started some other way");
  }
  const v: unknown = JSON.parse(raw);
  if (!Array.isArray(v) || !v.every(isRow)) {
    throw new Error(`skills: HARNESS_SKILLS is not a list of { name, description, source, path }: ${raw.slice(0, 200)}`);
  }
  return (known = v);
}

/** Every skill this agent was launched with, by name: the system note's index, with where each one's file is. */
export function skills(): Table<SkillRow> {
  return table(list());
}

/** A skill file's body: where it is (so it can be cited), then its text after the front matter. A folder skill
 *  (`<name>/SKILL.md`) says where its files are. */
export function body(s: SkillRow, text: string): string {
  const fm = /^---[ \t]*\r?\n[\s\S]*?\r?\n---[ \t]*(\r?\n|$)/.exec(text);
  if (!fm) throw new Error(`skill ${s.name}: ${s.path} no longer starts with front matter (a --- line, name and description, ---); fix the file`);
  const rest = text.slice(fm[0].length).trim();
  // Where it came from leads it, so what the agent says from it can point back at it (a chip the person opens).
  const from = `(This is the ${s.name} skill, ${s.path}. When you say something from it, cite it: <Path loc="${s.path}">${s.name} skill</Path>, with line={n} when you know the line.)`;
  const rel = s.path.endsWith("/SKILL.md") ? `\n(Paths this skill names are relative to ${dirname(s.path)}.)` : "";
  return `${from}${rel}\n\n${rest}`;
}

/** The full instructions of one skill, read now from its file. */
export async function skill(name: string): Promise<string> {
  const all = list();
  const s = all.find((k) => k.name === name);
  if (!s) throw new Error(`skill(${JSON.stringify(name)}): no skill by that name; the skills are ${all.map((k) => k.name).join(", ")}`);
  return effect("skill", `Reading skill ${name}`, `Read skill ${name}`, s.path, async () => body(s, await Bun.file(s.path).text()),
    (text) => ({ detail: bytes(Buffer.byteLength(text)) }));
}

export const skillDocs = {
  skill: "skill(name): Promise<string> — the full instructions of one skill, from the index at the end of your system note (read it before the work it covers); the person sees a quiet \"Read skill name\" step, not the text.",
  skills: "skills(): Table<{ name; description; source: harness|user|project; path }> — every skill you can load, by name.",
};
