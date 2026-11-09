//! The state of the pull requests and issues a chat's words name (`github_status`, the app's `<PR>` and `<Issue>` chips
//! and their hover cards): one GraphQL query for all of a chat's refs, run through `gh api graphql`, so the request
//! carries gh's own token and the secret never enters this process, its logs or its replies.
//!
//! The core ([`query`], [`parse`], [`Cache`]) is synchronous and owns no IO: it builds the query, reads the answer into
//! one [`GhItem`] a ref, and keeps answers for [`TTL`] seconds of an injected clock. The driver ([`status`]) runs `gh`
//! with a bounded wait ([`run_bounded`]). Nothing is guessed: a ref the answer does not cover comes back with an error
//! naming `owner/name#n`, its state unset.
//!
//! While a pull request's card shows and its checks are not all done, the app asks after them one by one
//! (`github_checks`: [`checks_query`], [`parse_checks`], kept [`CHECKS_TTL`] seconds by [`checks`]): each check run's or
//! commit status's state, start and end, page, and a running job's step in progress, read from the check run's own steps
//! in the same query, so a card costs one `gh` run however many jobs run.
use harness_proto::peek::{BODY_MAX, CHECKS_MAX, GhCheck, GhCheckState, GhChecks, GhCi, GhItem, GhKind, GhRef, GhState, REFS_MAX, STEPS_MAX};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long an answer is kept (seconds): a chat's chips ask again on focus (`fresh`) and every minute while it shows.
pub const TTL: f64 = 60.0;
/// How long a failure is kept before the next ask tries again.
pub const TTL_FAILED: f64 = 15.0;
/// The most `gh` may take before it is killed and every ref it held fails, naming the wait.
pub const GH_WAIT: Duration = Duration::from_secs(15);
/// The most of `gh`'s output read (a hundred refs' answers fit many times over).
const OUT_MAX: usize = 8 << 20;

/// `repo` split into its owner and name, when both are what GitHub allows (letters, digits, `.`, `_`, `-`): the only
/// text that goes into the query, so a ref cannot change what it asks.
fn owner_name(repo: &str) -> Option<(&str, &str)> {
    let (o, n) = repo.split_once('/')?;
    let ok = |s: &str| !s.is_empty() && s.len() <= 100 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    (ok(o) && ok(n)).then_some((o, n))
}

/// The aliases a ref's answer sits under: `r<k>` for its repository (the k-th distinct one), `i<j>` for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    pub repo: String,
    pub item: String,
}

/// The GraphQL document asking after `refs` (each valid one under its [`Alias`], in order), and the refs it cannot ask
/// (a repo not `owner/name`), each with its error.
#[must_use]
pub fn query(refs: &[GhRef]) -> (String, Vec<Option<Alias>>, Vec<(usize, String)>) {
    let mut repos: BTreeMap<&str, (usize, Vec<String>)> = BTreeMap::new();
    let mut aliases = Vec::with_capacity(refs.len());
    let mut bad = Vec::new();
    for (j, r) in refs.iter().enumerate() {
        let Some(_) = owner_name(&r.repo) else {
            bad.push((j, format!("{}#{}: not a GitHub repository (owner/name)", r.repo, r.n)));
            aliases.push(None);
            continue;
        };
        let next = repos.len();
        let (k, fields) = repos.entry(r.repo.as_str()).or_insert_with(|| (next, Vec::new()));
        let (field, frag) = match r.kind {
            GhKind::Pr => ("pullRequest", "Pr"),
            GhKind::Issue => ("issue", "Is"),
        };
        fields.push(format!("i{j}: {field}(number: {}) {{ ...{frag} }}", r.n));
        aliases.push(Some(Alias { repo: format!("r{k}"), item: format!("i{j}") }));
    }
    let mut by_k: Vec<(&usize, &str, &Vec<String>)> = repos.iter().map(|(repo, (k, f))| (k, *repo, f)).collect();
    by_k.sort_by_key(|(k, ..)| **k);
    let mut doc = String::from("query {\n");
    for (k, repo, fields) in by_k {
        let (o, n) = owner_name(repo).expect("checked above");
        doc.push_str(&format!("  r{k}: repository(owner: \"{o}\", name: \"{n}\") {{\n"));
        for f in fields {
            doc.push_str(&format!("    {f}\n"));
        }
        doc.push_str("  }\n");
    }
    doc.push_str("}\n");
    // Only the fragments the document spreads: GitHub refuses the whole query over one defined but unused.
    if refs.iter().zip(&aliases).any(|(r, a)| a.is_some() && r.kind == GhKind::Pr) {
        doc.push_str(
            "fragment Pr on PullRequest { title state isDraft merged url additions deletions body author { login } \
             commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } }\n",
        );
    }
    if refs.iter().zip(&aliases).any(|(r, a)| a.is_some() && r.kind == GhKind::Issue) {
        doc.push_str("fragment Is on Issue { title state stateReason url body author { login } }\n");
    }
    (doc, aliases, bad)
}

/// `refs`' items from `answer`, GitHub's reply to [`query`]'s document: each one's fields, or the error the reply gives
/// for its path (its repository's, else its own), or an error saying the reply has nothing for it.
#[must_use]
pub fn parse(refs: &[GhRef], aliases: &[Option<Alias>], bad: &[(usize, String)], answer: &Value) -> Vec<GhItem> {
    let errors: Vec<(Vec<String>, String)> = answer["errors"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    let path = e["path"].as_array().map(|p| p.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                    (path, e["message"].as_str().unwrap_or("GitHub refused the query").to_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    refs.iter()
        .enumerate()
        .map(|(j, r)| {
            let name = format!("{}#{}", r.repo, r.n);
            if let Some((_, why)) = bad.iter().find(|(b, _)| *b == j) {
                return GhItem::failed(r, why.clone());
            }
            let Some(a) = &aliases[j] else { return GhItem::failed(r, format!("{name}: not asked")) };
            let node = &answer["data"][&a.repo][&a.item];
            if node.is_object() {
                return item(r, node);
            }
            let said = errors
                .iter()
                .find(|(p, _)| p.len() >= 2 && p[0] == a.repo && p[1] == a.item)
                .or_else(|| errors.iter().find(|(p, _)| p.len() == 1 && p[0] == a.repo))
                .or_else(|| errors.iter().find(|(p, _)| p.is_empty()))
                // No data at all: the query itself was refused (its error's path names no ref, e.g. a fragment).
                .or_else(|| answer["data"].is_null().then(|| errors.first()).flatten())
                .map(|(_, m)| m.clone());
            let kind = match r.kind {
                GhKind::Pr => "pull request",
                GhKind::Issue => "issue",
            };
            GhItem::failed(r, format!("{name}: {}", said.unwrap_or_else(|| format!("GitHub's answer has no such {kind}"))))
        })
        .collect()
}

/// One answered node as an item.
fn item(r: &GhRef, n: &Value) -> GhItem {
    let s = |k: &str| n[k].as_str().unwrap_or_default().to_owned();
    let state = match (r.kind, n["state"].as_str()) {
        (GhKind::Pr, Some("MERGED")) => Some(GhState::Merged),
        (GhKind::Pr, Some("CLOSED")) => Some(GhState::Closed),
        (GhKind::Pr, Some("OPEN")) if n["isDraft"].as_bool() == Some(true) => Some(GhState::Draft),
        (GhKind::Pr, Some("OPEN")) => Some(GhState::Open),
        (GhKind::Issue, Some("OPEN")) => Some(GhState::Open),
        (GhKind::Issue, Some("CLOSED")) if n["stateReason"].as_str() == Some("NOT_PLANNED") => Some(GhState::NotPlanned),
        (GhKind::Issue, Some("CLOSED")) => Some(GhState::Closed),
        _ => None,
    };
    let ci = match n["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"].as_str() {
        Some("SUCCESS") => Some(GhCi::Success),
        Some("FAILURE" | "ERROR") => Some(GhCi::Failure),
        Some("PENDING" | "EXPECTED") => Some(GhCi::Pending),
        _ => None,
    };
    let mut it = GhItem::of(r);
    it.state = state;
    it.ci = ci;
    it.title = s("title");
    it.url = s("url");
    it.author = n["author"]["login"].as_str().unwrap_or_default().to_owned();
    it.additions = n["additions"].as_u64().unwrap_or(0);
    it.deletions = n["deletions"].as_u64().unwrap_or(0);
    it.body = cut(&s("body"), BODY_MAX);
    if state.is_none() {
        it.error = format!("{}#{}: GitHub gave a state this does not know ({})", r.repo, r.n, n["state"]);
    }
    it
}

/// `s` cut to at most `max` bytes on a character boundary.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Answers by ref, each with when (the injected clock's seconds) it was fetched.
#[derive(Default)]
pub struct Cache {
    kept: BTreeMap<GhRef, (f64, GhItem)>,
}

impl Cache {
    /// What is kept for `r` and still fresh at `now`.
    #[must_use]
    pub fn get(&self, r: &GhRef, now: f64) -> Option<&GhItem> {
        let (at, it) = self.kept.get(r)?;
        let ttl = if it.error.is_empty() { TTL } else { TTL_FAILED };
        (now - at < ttl).then_some(it)
    }

    /// Keep `items` as fetched at `now`, dropping what has gone stale.
    pub fn put(&mut self, items: &[GhItem], now: f64) {
        self.kept.retain(|_, (at, _)| now - *at < TTL);
        for it in items {
            self.kept.insert(it.at(), (now, it.clone()));
        }
    }
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn now() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// `refs`' items, in order: from the cache unless `fresh`, the rest from one `gh api graphql` run. Refused past
/// [`REFS_MAX`] refs, naming how many.
pub fn status(refs: &[GhRef], fresh: bool) -> anyhow::Result<Vec<GhItem>> {
    if refs.len() > REFS_MAX {
        anyhow::bail!("github_status: {} refs asked, at most {REFS_MAX} a request", refs.len());
    }
    let t = now();
    let mut out: Vec<Option<GhItem>> = {
        let c = CACHE.lock();
        refs.iter().map(|r| if fresh { None } else { c.as_ref().and_then(|c| c.get(r, t)).cloned() }).collect()
    };
    let missing: Vec<GhRef> = refs.iter().zip(&out).filter(|(_, o)| o.is_none()).map(|(r, _)| r.clone()).collect();
    if !missing.is_empty() {
        let got = fetch(&missing);
        CACHE.lock().get_or_insert_with(Cache::default).put(&got, now());
        let mut got = got.into_iter();
        for o in out.iter_mut().filter(|o| o.is_none()) {
            *o = got.next();
        }
    }
    Ok(out.into_iter().zip(refs).map(|(o, r)| o.unwrap_or_else(|| GhItem::failed(r, format!("{}#{}: not answered", r.repo, r.n)))).collect())
}

/// `refs` asked of GitHub through `gh`; a failure of the run itself fails each ref, naming it and why.
fn fetch(refs: &[GhRef]) -> Vec<GhItem> {
    let (doc, aliases, bad) = query(refs);
    let body = json!({ "query": doc }).to_string();
    let fail_all = |why: &str| -> Vec<GhItem> { refs.iter().map(|r| GhItem::failed(r, format!("{}#{}: {why}", r.repo, r.n))).collect() };
    if aliases.iter().all(Option::is_none) {
        return parse(refs, &aliases, &bad, &Value::Null);
    }
    let mut cmd = Command::new("gh");
    cmd.args(["api", "graphql", "--input", "-"]);
    let run = match run_bounded(cmd, body.as_bytes(), GH_WAIT, "gh api graphql") {
        Ok(r) => r,
        Err(e) => return fail_all(&e),
    };
    // gh prints GitHub's answer on stdout even when it holds errors (and then exits 1): read that first.
    match serde_json::from_slice::<Value>(&run.stdout) {
        Ok(v) if v.get("data").is_some() || v.get("errors").is_some() => parse(refs, &aliases, &bad, &v),
        _ => {
            let err = String::from_utf8_lossy(&run.stderr);
            let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("no output");
            fail_all(&format!("gh api graphql failed (exit {}): {line}", run.code.map_or("signal".to_owned(), |c| c.to_string())))
        }
    }
}

/// How long a pull request's checks one by one are kept (seconds): long enough that the cards of several windows share
/// one ask, short enough that a step in progress is the one running now.
pub const CHECKS_TTL: f64 = 10.0;

/// The GraphQL document asking after pull request `r`'s head commit's checks one by one (at most [`CHECKS_MAX`], each
/// job's steps to find the one in progress), or why it cannot be asked.
pub fn checks_query(r: &GhRef) -> Result<String, String> {
    if r.kind != GhKind::Pr {
        return Err(format!("{}#{}: an issue has no checks", r.repo, r.n));
    }
    let Some((o, n)) = owner_name(&r.repo) else { return Err(format!("{}#{}: not a GitHub repository (owner/name)", r.repo, r.n)) };
    Ok(format!(
        "query {{ repository(owner: \"{o}\", name: \"{n}\") {{ pullRequest(number: {}) {{ commits(last: 1) {{ nodes {{ commit {{ \
         statusCheckRollup {{ contexts(first: {CHECKS_MAX}) {{ totalCount nodes {{ __typename \
         ... on CheckRun {{ name status conclusion startedAt completedAt detailsUrl \
         checkSuite {{ workflowRun {{ workflow {{ name }} }} }} steps(first: {STEPS_MAX}) {{ nodes {{ name status }} }} }} \
         ... on StatusContext {{ context state targetUrl createdAt }} }} }} }} }} }} }} }} }} }}\n",
        r.n
    ))
}

/// Pull request `r`'s checks from `answer`, GitHub's reply to [`checks_query`]'s document, in GitHub's order; a commit
/// with no checks has none, and a reply without the pull request is its error, naming it.
#[must_use]
pub fn parse_checks(r: &GhRef, answer: &Value) -> GhChecks {
    let pr = &answer["data"]["repository"]["pullRequest"];
    if !pr.is_object() {
        let said = answer["errors"][0]["message"].as_str().unwrap_or("GitHub's answer has no such pull request");
        return GhChecks::failed(r, format!("{}#{}: {said}", r.repo, r.n));
    }
    let contexts = &pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
    let s = |n: &Value, k: &str| n[k].as_str().unwrap_or_default().to_owned();
    let checks: Vec<GhCheck> = contexts["nodes"]
        .as_array()
        .map(|a| a.iter().take(CHECKS_MAX))
        .into_iter()
        .flatten()
        .filter_map(|n| match n["__typename"].as_str()? {
            "CheckRun" => {
                let state = match (n["status"].as_str()?, n["conclusion"].as_str()) {
                    ("COMPLETED", Some("SUCCESS")) => GhCheckState::Passed,
                    ("COMPLETED", Some("NEUTRAL" | "SKIPPED" | "STALE")) => GhCheckState::Skipped,
                    ("COMPLETED", _) => GhCheckState::Failed,
                    ("IN_PROGRESS", _) => GhCheckState::Running,
                    _ => GhCheckState::Queued,
                };
                let step = (state == GhCheckState::Running)
                    .then(|| n["steps"]["nodes"].as_array()?.iter().find(|st| st["status"].as_str() == Some("IN_PROGRESS")).map(|st| s(st, "name")))
                    .flatten()
                    .unwrap_or_default();
                Some(GhCheck {
                    name: s(n, "name"),
                    state,
                    workflow: s(&n["checkSuite"]["workflowRun"]["workflow"], "name"),
                    step,
                    started: unix(n["startedAt"].as_str().unwrap_or_default()),
                    completed: unix(n["completedAt"].as_str().unwrap_or_default()),
                    url: s(n, "detailsUrl"),
                })
            }
            "StatusContext" => {
                let state = match n["state"].as_str()? {
                    "SUCCESS" => GhCheckState::Passed,
                    "FAILURE" | "ERROR" => GhCheckState::Failed,
                    "PENDING" => GhCheckState::Running,
                    _ => GhCheckState::Queued,
                };
                Some(GhCheck {
                    name: s(n, "context"),
                    state,
                    workflow: String::new(),
                    step: String::new(),
                    started: unix(n["createdAt"].as_str().unwrap_or_default()),
                    completed: 0,
                    url: s(n, "targetUrl"),
                })
            }
            _ => None,
        })
        .collect();
    let total = contexts["totalCount"].as_u64().unwrap_or(0).max(checks.len() as u64);
    GhChecks { checks, total, ..GhChecks::of(r) }
}

/// An RFC 3339 time in UTC as GitHub gives it (`2026-11-07T00:15:34Z`) as Unix seconds; 0 for anything else.
#[must_use]
pub fn unix(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || !s.ends_with('Z') {
        return 0;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|t| t.parse::<i64>().ok());
    let (Some(y), Some(m), Some(d), Some(hh), Some(mm), Some(ss)) = (num(0..4), num(5..7), num(8..10), num(11..13), num(14..16), num(17..19)) else {
        return 0;
    };
    // Days from 1970-01-01 to y-m-d in the proleptic Gregorian calendar (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hh * 3600 + mm * 60 + ss
}

static CHECKS: Mutex<BTreeMap<GhRef, (f64, GhChecks)>> = Mutex::new(BTreeMap::new());

/// Pull request `r`'s checks one by one: kept for [`CHECKS_TTL`] seconds, else from one `gh api graphql` run; a failure
/// is the answer's error, naming the pull request and why.
pub fn checks(r: &GhRef) -> GhChecks {
    let t = now();
    if let Some((at, c)) = CHECKS.lock().get(r)
        && t - at < CHECKS_TTL
    {
        return c.clone();
    }
    let got = fetch_checks(r);
    let mut kept = CHECKS.lock();
    kept.retain(|_, (at, _)| t - *at < CHECKS_TTL);
    kept.insert(r.clone(), (t, got.clone()));
    got
}

fn fetch_checks(r: &GhRef) -> GhChecks {
    let doc = match checks_query(r) {
        Ok(d) => d,
        Err(why) => return GhChecks::failed(r, why),
    };
    let mut cmd = Command::new("gh");
    cmd.args(["api", "graphql", "--input", "-"]);
    let run = match run_bounded(cmd, json!({ "query": doc }).to_string().as_bytes(), GH_WAIT, "gh api graphql") {
        Ok(run) => run,
        Err(e) => return GhChecks::failed(r, format!("{}#{}: {e}", r.repo, r.n)),
    };
    match serde_json::from_slice::<Value>(&run.stdout) {
        Ok(v) if v.get("data").is_some() || v.get("errors").is_some() => parse_checks(r, &v),
        _ => {
            let err = String::from_utf8_lossy(&run.stderr);
            let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("no output");
            let code = run.code.map_or("signal".to_owned(), |c| c.to_string());
            GhChecks::failed(r, format!("{}#{}: gh api graphql failed (exit {code}): {line}", r.repo, r.n))
        }
    }
}

/// What a bounded run printed and how it ended.
pub struct Ran {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
}

/// Run `cmd` with `input` on its stdin, reading at most [`OUT_MAX`] of each output, killed at `wait`: its outputs and
/// exit, or an error naming `what` (not installed, could not start, killed at the deadline).
pub fn run_bounded(mut cmd: Command, input: &[u8], wait: Duration, what: &str) -> Result<Ran, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { format!("{what}: not installed") } else { format!("{what}: {e}") })?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec(); // alloc: the request body, handed to the writer thread
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let read = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new(); // alloc: the child's output, returned
            if let Some(r) = r {
                let _ = r.take(OUT_MAX as u64).read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = read(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let err = read(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let deadline = Instant::now() + wait;
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{what}: no answer in {}s, killed", wait.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("{what}: {e}")),
        }
    };
    let _ = writer.join();
    Ok(Ran { stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default(), code })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(repo: &str, n: u64) -> GhRef {
        GhRef { kind: GhKind::Pr, repo: repo.into(), n }
    }

    /// One query for all refs, a repository alias each, refs of one repository under it; a bad repo never reaches it.
    #[test]
    fn query_batches_by_repository() {
        let refs = [pr("indexable-inc/ix", 7652), GhRef { kind: GhKind::Issue, repo: "a/b".into(), n: 3 }, pr("indexable-inc/ix", 1), pr("x\"){evil}", 2)];
        let (doc, aliases, bad) = query(&refs);
        assert_eq!(doc.matches("repository(").count(), 2);
        assert!(doc.contains("r0: repository(owner: \"indexable-inc\", name: \"ix\")"));
        assert!(doc.contains("i2: pullRequest(number: 1) { ...Pr }"));
        assert!(doc.contains("i1: issue(number: 3) { ...Is }"));
        assert!(!doc.contains("evil"));
        assert_eq!(aliases[2], Some(Alias { repo: "r0".into(), item: "i2".into() }));
        assert_eq!(bad.len(), 1);
        assert!(bad[0].1.starts_with("x\"){evil}#2: not a GitHub repository"));
        assert!(doc.contains("fragment Pr on PullRequest") && doc.contains("fragment Is on Issue"));
        // GitHub refuses a query that defines a fragment it never spreads: pull requests alone define only Pr.
        let (prs, ..) = query(&refs[..1]);
        assert!(prs.contains("fragment Pr on PullRequest") && !prs.contains("fragment Is"));
        let (issues, ..) = query(&refs[1..2]);
        assert!(issues.contains("fragment Is on Issue") && !issues.contains("fragment Pr"));
    }

    /// The states and the checks as GitHub says them; an error under a ref's path, or none at all, names it.
    #[test]
    fn answers_become_items() {
        let refs = [pr("o/a", 1), pr("o/a", 2), GhRef { kind: GhKind::Issue, repo: "o/a".into(), n: 3 }, pr("o/a", 4), pr("o/gone", 5)];
        let (_, aliases, bad) = query(&refs);
        let answer = json!({
            "data": {"r0": {
                "i0": {"title": "Fix", "state": "OPEN", "isDraft": false, "url": "u", "additions": 3, "deletions": 1,
                       "author": {"login": "ana"}, "body": "b",
                       "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "ERROR"}}}]}},
                "i1": {"title": "WIP", "state": "OPEN", "isDraft": true, "commits": {"nodes": [{"commit": {"statusCheckRollup": null}}]}},
                "i2": {"title": "Bug", "state": "CLOSED", "stateReason": "NOT_PLANNED"},
                "i3": null
            }, "r1": null},
            "errors": [
                {"path": ["r0", "i3"], "message": "Could not resolve to a PullRequest with the number of 4."},
                {"path": ["r1"], "message": "Could not resolve to a Repository with the name 'o/gone'."}
            ]
        });
        let items = parse(&refs, &aliases, &bad, &answer);
        assert_eq!((items[0].state, items[0].ci, items[0].author.as_str()), (Some(GhState::Open), Some(GhCi::Failure), "ana"));
        assert_eq!((items[1].state, items[1].ci), (Some(GhState::Draft), None));
        assert_eq!(items[2].state, Some(GhState::NotPlanned));
        assert_eq!(items[3].state, None);
        assert_eq!(items[3].error, "o/a#4: Could not resolve to a PullRequest with the number of 4.");
        assert!(items[4].error.starts_with("o/gone#5: Could not resolve to a Repository"));
        // No answer at all: each ref fails by name, none guessed.
        let none = parse(&refs[..1], &aliases[..1], &[], &Value::Null);
        assert_eq!((none[0].state, none[0].error.as_str()), (None, "o/a#1: GitHub's answer has no such pull request"));
        // A refused query (no data, an error on no ref's path) names GitHub's reason, not a missing pull request.
        let refused = json!({"errors": [{"path": ["fragment Is"], "message": "Fragment Is was defined, but not used"}]});
        let r = parse(&refs[..1], &aliases[..1], &[], &refused);
        assert_eq!(r[0].error, "o/a#1: Fragment Is was defined, but not used");
    }

    /// Each check run and status as a check (GitHub's shape, from a real answer), a running job's step the one in
    /// progress; a pull request the answer lacks is its error, by name; an issue or a bad repository is never asked.
    #[test]
    fn checks_one_by_one() {
        let r = pr("indexable-inc/blaze", 1872);
        let doc = checks_query(&r).expect("a pull request");
        assert!(doc.contains("repository(owner: \"indexable-inc\", name: \"blaze\") { pullRequest(number: 1872)"));
        assert!(doc.contains(&format!("contexts(first: {CHECKS_MAX})")) && doc.contains(&format!("steps(first: {STEPS_MAX})")));
        assert!(checks_query(&GhRef { kind: GhKind::Issue, repo: "o/a".into(), n: 1 }).is_err());
        assert!(checks_query(&pr("x\"){evil}", 2)).is_err());
        let answer = json!({"data": {"repository": {"pullRequest": {"commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
            "totalCount": 60,
            "nodes": [
                {"__typename": "CheckRun", "name": "Analyze (rust)", "status": "IN_PROGRESS", "conclusion": null,
                 "startedAt": "2026-11-07T00:15:34Z", "completedAt": null,
                 "detailsUrl": "https://github.com/o/a/actions/runs/1/job/2", "checkSuite": {"workflowRun": {"workflow": {"name": "CodeQL"}}},
                 "steps": {"nodes": [{"name": "Set up job", "status": "COMPLETED"}, {"name": "Perform CodeQL Analysis", "status": "IN_PROGRESS"}]}},
                {"__typename": "CheckRun", "name": "tango", "status": "QUEUED", "conclusion": null, "startedAt": "2026-11-07T00:15:34Z",
                 "completedAt": null, "detailsUrl": "u", "checkSuite": {"workflowRun": null}, "steps": {"nodes": []}},
                {"__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "CANCELLED", "startedAt": null,
                 "completedAt": null, "detailsUrl": "", "checkSuite": null, "steps": {"nodes": []}},
                {"__typename": "CheckRun", "name": "CodeQL", "status": "COMPLETED", "conclusion": "NEUTRAL", "startedAt": null,
                 "completedAt": null, "detailsUrl": "", "checkSuite": null, "steps": {"nodes": []}},
                {"__typename": "StatusContext", "context": "ci/ext", "state": "PENDING", "targetUrl": "t", "createdAt": "1970-01-01T00:01:00Z"}
            ]}}}}]}}}}});
        let c = parse_checks(&r, &answer);
        assert_eq!((c.total, c.checks.len(), c.error.as_str()), (60, 5, ""));
        let run = &c.checks[0];
        assert_eq!((run.state, run.step.as_str(), run.workflow.as_str()), (GhCheckState::Running, "Perform CodeQL Analysis", "CodeQL"));
        assert_eq!((run.started, run.completed), (1_794_010_534, 0));
        let states: Vec<GhCheckState> = c.checks.iter().map(|c| c.state).collect();
        assert_eq!(states, [GhCheckState::Running, GhCheckState::Queued, GhCheckState::Failed, GhCheckState::Skipped, GhCheckState::Running]);
        assert_eq!((c.checks[1].step.as_str(), c.checks[4].name.as_str(), c.checks[4].started), ("", "ci/ext", 60));
        assert!(c.unfinished());
        let gone = parse_checks(&r, &json!({"data": {"repository": {"pullRequest": null}}, "errors": [{"message": "Could not resolve"}]}));
        assert_eq!(gone.error, "indexable-inc/blaze#1872: Could not resolve");
    }

    /// GitHub's times as Unix seconds; anything else is 0.
    #[test]
    fn times_are_unix_seconds() {
        assert_eq!(unix("1970-01-01T00:00:00Z"), 0);
        assert_eq!(unix("2000-03-01T00:00:00Z"), 951_868_800);
        assert_eq!(unix("2026-11-07T00:15:34Z"), 1_794_010_534);
        assert_eq!(unix(""), 0);
        assert_eq!(unix("2026-11-07 00:15:34"), 0);
    }

    /// Kept for TTL seconds of the injected clock, a failure for TTL_FAILED.
    #[test]
    fn cache_keeps_for_its_ttl() {
        let mut c = Cache::default();
        let ok = GhItem { state: Some(GhState::Merged), ..GhItem::of(&pr("o/a", 1)) };
        let bad = GhItem::failed(&pr("o/a", 2), "o/a#2: x".into());
        c.put(&[ok, bad], 100.0);
        assert!(c.get(&pr("o/a", 1), 100.0 + TTL - 1.0).is_some());
        assert!(c.get(&pr("o/a", 1), 100.0 + TTL).is_none());
        assert!(c.get(&pr("o/a", 2), 100.0 + TTL_FAILED - 1.0).is_some());
        assert!(c.get(&pr("o/a", 2), 100.0 + TTL_FAILED).is_none());
    }
}
