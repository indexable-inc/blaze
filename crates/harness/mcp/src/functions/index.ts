// Every function the harness exposes: the kernel puts each one in a cell's scope by name, and `docs` is what help() says
// about them (each module's own, docs.ts).
import { googleSearch, docs as googleDocs } from "./google.ts";
import { gmail, docs as gmailDocs } from "./gmail.ts";
import { github, docs as githubDocs } from "./github.ts";
import { browser, location, places, docs as browserDocs } from "./browser.ts";
import { withHiddenPage, docs as cdpDocs } from "../cdp.ts";
import { table, Table, LazyTable, col, lit, docs as tableDocs } from "../table.ts";
import { gitLog, docs as gitLogDocs } from "../git-log.ts";
import { nuon, docs as nuonDocs } from "../nuon.ts";
import { tasks, task, taskValue, docs as taskDocs } from "../tasks.ts";
import { wait, more, rerun, cancel, signal, docs as cellDocs } from "../cells.ts";
import { progress, docs as progressDocs } from "../effects.ts";
import { agents, docs as agentsDocs } from "../agents.ts";
import { sql, docs as sqlDocs } from "../sql.ts";
import { spawn, obj, objects, tree, docs as objDocs } from "../obj.ts";
import { rg, ls, glob, du, docs as searchDocs } from "../search.ts";
import { $, docs as dollarDocs } from "../dollar.ts";
import { follow, cargo, docs as followDocs } from "../follow.ts";
import { pmap, docs as pmapDocs } from "../pmap.ts";
import { image, docs as imageDocs } from "../image.ts";
import { vfs, docs as vfsDocs } from "../vfs.ts";
import { sh, procs, out, err, lines, kill, ptree, runs, stage, runInfo, docs as procDocs } from "../proc.ts";
import { skill, skills, docs as skillDocs } from "../skills.ts";
import { world, docs as worldDocs } from "../world.ts";
import { phone, docs as phoneDocs } from "../phone.ts";
import { call, docs as callDocs } from "../call.ts";
import { uiDocs, type UiGlobals } from "../ui/index.ts";
import { makeHelp } from "../help.ts";
import type { Docs } from "../docs.ts";

export const docs: Docs = {
  ...cellDocs, ...taskDocs, ...progressDocs, ...pmapDocs, ...skillDocs, ...searchDocs, ...gitLogDocs, ...imageDocs, ...vfsDocs, ...dollarDocs, ...procDocs,
  ...followDocs, ...tableDocs, ...nuonDocs, ...sqlDocs, ...googleDocs, ...browserDocs, ...cdpDocs, ...githubDocs, ...gmailDocs, ...objDocs, ...agentsDocs,
  ...uiDocs, ...worldDocs, ...phoneDocs, ...callDocs,
};

export const help = makeHelp(docs);

export const functions = {
  help, more, rerun, cancel, signal, progress, skill, skills, rg, ls, glob, du, cargo, follow, $, pmap, image, vfs, spawn, obj, objects, tree, sql, googleSearch, gmail, github,
  browser, location, places, withHiddenPage, table, Table, LazyTable, col, lit, gitLog, nuon, tasks, task, taskValue, wait, sh, procs, out, err, lines, kill, ptree, runs,
  stage, runInfo, agents, world, phone, call,
};

/** Every global a cell's TypeScript is checked against (typecheck.ts), by name. */
export type Globals = typeof functions & UiGlobals;
