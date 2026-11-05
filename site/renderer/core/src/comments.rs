//! Comments as part of a discussable page, as render/comments.ts. A comment is an entity with a `Comment` value, a
//! child of its block's entity, or of the page's for `_page` and for blocks gone from the page; each block's thread
//! is drawn from those children. Every piece of user text goes through esc().

use std::sync::LazyLock;

use regex_lite::Regex;
use serde::Deserialize;

use crate::inline::esc;
use crate::js;
use crate::render::Served;
use crate::world::{Entity, Id, Scalar, Value, World};

pub const PAGE_BLOCK: &str = "_page";
pub const COMMENT: &str = "Comment";
pub const BODY_MAX: usize = 4000;

/// A comment as the host reads it from its store (D1's `comments` joined with `users`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Comment {
    pub id: i64,
    pub block: String,
    #[serde(default)]
    pub quote: String,
    pub body: String,
    pub created_at: String,
    pub user_id: i64,
    pub login: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub avatar_url: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Viewer {
    pub id: i64,
    pub login: String,
    #[serde(default)]
    pub admin: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct FormError {
    pub block: String,
    pub message: String,
}

/// Who the page is rendered for. No viewer: anonymous, the HTML may be shared (cached).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Discussion {
    #[serde(default)]
    pub viewer: Option<Viewer>,
    /// GitHub sign-in is set up (else the form says comments are not set up).
    #[serde(default = "yes", alias = "signIn")]
    pub sign_in: bool,
    /// A failed no-JS post or delete, shown in that block's form.
    #[serde(default)]
    pub error: Option<FormError>,
}

fn yes() -> bool {
    true
}

impl Default for Discussion {
    /// Anonymous, sign-in set up, no comments: what the build writes to dist/.
    fn default() -> Discussion {
        Discussion { viewer: None, sign_in: true, error: None }
    }
}

impl Comment {
    pub fn value(&self) -> Value {
        let s = |v: &str| Scalar::Str(v.to_string());
        Value::from([
            ("id".into(), Scalar::Int(self.id)),
            ("block".into(), s(&self.block)),
            ("quote".into(), s(&self.quote)),
            ("body".into(), s(&self.body)),
            ("created_at".into(), s(&self.created_at)),
            ("user_id".into(), Scalar::Int(self.user_id)),
            ("login".into(), s(&self.login)),
            ("name".into(), s(&self.name)),
            ("avatar_url".into(), s(&self.avatar_url)),
        ])
    }

    pub fn entity(&self) -> Entity {
        Entity {
            name: format!("comment_{}", self.id),
            values: vec![(COMMENT.into(), self.value())],
            ..Entity::default()
        }
    }

    fn from_value(v: &Value) -> Comment {
        let s = |k: &str| js::string(v.get(k));
        let i = |k: &str| match v.get(k) {
            Some(Scalar::Int(i)) => *i,
            _ => 0,
        };
        Comment {
            id: i("id"),
            block: s("block"),
            quote: s("quote"),
            body: s("body"),
            created_at: s("created_at"),
            user_id: i("user_id"),
            login: s("login"),
            name: s("name"),
            avatar_url: s("avatar_url"),
        }
    }
}

pub fn is_comment(e: &Entity) -> bool {
    e.has(COMMENT)
}

/// Spawns each comment under its block (a path under the page) or under the page. Blocks are looked up among the
/// page's own entities, never among comments spawned here.
/// Spawns `comments` for one render, as worker/pages.ts `withComments` hangs them: under their block when that block
/// renders a thread (`threads`, the page's blocks from a static render), else under the page.
pub fn spawn(
    world: &mut World,
    page: Id,
    threads: &[(String, String)],
    comments: &[Comment],
) -> Result<(), String> {
    let base = world.get(page).path.clone();
    let homes: Vec<Id> = comments
        .iter()
        .map(|c| {
            if c.block == PAGE_BLOCK || !threads.iter().any(|(id, _)| *id == c.block) {
                return page;
            }
            world
                .lookup(&format!("{base}.{}", c.block))
                .filter(|b| !is_comment(world.get(*b)))
                .unwrap_or(page)
        })
        .collect();
    for (c, home) in comments.iter().zip(homes) {
        world.spawn(Some(home), c.entity())?;
    }
    Ok(())
}

// ---- Body: paragraphs on blank lines, `code`, http(s) autolinks, line breaks; everything else text. ----

static PARAS: LazyLock<Regex> = LazyLock::new(|| Regex::new("\n[ \t]*\n+").expect("compiles"));
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new("`[^`\n]+`").expect("compiles"));
static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("https?://[^{}<>\"'`]+", js::SPACE_CLASS)).expect("compiles")
});

/// A URL without the punctuation that usually ends the sentence around it (keeps balanced parens).
fn trim_url(url: &str) -> &str {
    let mut u = url;
    loop {
        let Some(last) = u.chars().last() else { return u };
        let count = |c: char| u.chars().filter(|x| *x == c).count();
        if ".,;:!?'\"".contains(last)
            || (last == ')' && count('(') < count(')'))
            || (last == ']' && !u.contains('['))
        {
            u = &u[..u.len() - 1];
        } else {
            return u;
        }
    }
}

fn lines(t: &str) -> String {
    t.split('\n').map(esc).collect::<Vec<_>>().join("<br />")
}

fn linked(text: &str) -> String {
    let mut html = String::new();
    let mut at = 0;
    for m in URL.find_iter(text) {
        // A match starts at or after `at`: trimming only shortens the URL.
        let url = trim_url(m.as_str());
        html += &lines(&text[at..m.start()]);
        html += &format!("<a href=\"{0}\" rel=\"nofollow ugc noopener\">{0}</a>", esc(url));
        at = m.start() + url.len();
    }
    html + &lines(&text[at..])
}

/// `s.split(/(re)/)`: the pieces between matches, with each match in between.
fn split_keep<'a>(re: &Regex, s: &'a str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut at = 0;
    for m in re.find_iter(s) {
        out.push(&s[at..m.start()]);
        out.push(m.as_str());
        at = m.end();
    }
    out.push(&s[at..]);
    out
}

pub fn body_html(body: &str) -> String {
    let paras: Vec<String> = PARAS
        .split(js::trim(body))
        .map(|para| {
            let html: String = split_keep(&CODE, js::trim(para))
                .into_iter()
                .map(|part| {
                    if part.encode_utf16().count() > 2
                        && part.starts_with('`')
                        && part.ends_with('`')
                    {
                        format!("<code>{}</code>", esc(&part[1..part.len() - 1]))
                    } else if part.is_empty() {
                        String::new()
                    } else {
                        linked(part)
                    }
                })
                .collect();
            format!("<p>{html}</p>")
        })
        .collect();
    format!("<div class=\"c-body\">{}</div>", paras.concat())
}

/// "2026-10-24 19:47 UTC"; the client shows it relative. Absolute, so a cached page stays right.
fn when(iso: &str) -> String {
    match js::parse_date(iso) {
        Some(ms) => {
            let (day, minute) = js::iso_day_minute(ms);
            format!("{day} {minute} UTC")
        }
        None => String::new(),
    }
}

fn avatar_ok(url: &str) -> bool {
    url.strip_prefix("https://avatars.githubusercontent.com/")
        .is_some_and(|rest| rest.bytes().all(|b| (0x21..=0x7e).contains(&b)))
}

fn github(login: &str) -> String {
    format!("https://github.com/{}", js::encode_uri_component(login))
}

fn item(c: &Comment, block_id: &str, d: &Discussion, served: &Served) -> String {
    let avatar = if avatar_ok(&c.avatar_url) {
        c.avatar_url.clone()
    } else {
        format!("{}.png?size=48", github(&c.login))
    };
    let del = match &d.viewer {
        Some(v) if v.admin || v.id == c.user_id => format!(
            "<form class=\"c-delform\" method=\"post\" action=\"{}\"><button type=\"submit\" class=\"c-del\" aria-label=\"Delete comment by {}\">Delete</button></form>",
            esc(&served.href(&format!("/api/comments/{}/delete", c.id))),
            esc(&c.login)
        ),
        _ => String::new(),
    };
    let title = if !c.name.is_empty() && c.name != c.login {
        format!(" title=\"{}\"", esc(&c.name))
    } else {
        String::new()
    };
    let quote = if c.block != block_id && !c.quote.is_empty() {
        format!("<div class=\"c-quote\">{}</div>", esc(&c.quote))
    } else {
        String::new()
    };
    format!(
        "<article class=\"c-item\" id=\"c-{id}\"><img src=\"{avatar}\" alt=\"\" width=\"24\" height=\"24\" loading=\"lazy\" referrerpolicy=\"no-referrer\" /><div class=\"c-main\"><div class=\"c-head\"><a href=\"{gh}\" rel=\"nofollow noopener\"{title}>{login}</a> · <a class=\"c-when\" href=\"#c-{id}\"><time datetime=\"{at}\">{when}</time></a>{del}</div>{quote}{body}</div></article>",
        id = c.id,
        avatar = esc(&avatar),
        gh = esc(&github(&c.login)),
        login = esc(&c.login),
        at = esc(&c.created_at),
        when = esc(&when(&c.created_at)),
        body = body_html(&c.body),
    )
}

fn count_label(n: usize) -> String {
    match n {
        0 => "Comment on this".into(),
        1 => "1 comment".into(),
        n => format!("{n} comments"),
    }
}

fn comments_of(world: &World, n: Id) -> Vec<Comment> {
    world
        .get(n)
        .children
        .iter()
        .filter_map(|c| world.get(*c).value(COMMENT))
        .map(Comment::from_value)
        .collect()
}

/// The count button inside a block (none for `_page`).
pub fn button(block_id: &str, world: &World, n: Id) -> String {
    let count = world.get(n).children.iter().filter(|c| is_comment(world.get(**c))).count();
    let label = count_label(count);
    let shown = if count == 0 { "+".to_string() } else { count.to_string() };
    format!(
        "<a class=\"c-btn{}\" href=\"#block-{id}\" aria-controls=\"block-{id}\" aria-label=\"{label}\" title=\"{label}\">{shown}</a>",
        if count > 0 { " has" } else { "" },
        id = esc(block_id),
    )
}

/// A block's thread: its comments, then the form (or the sign-in link). `url` is the page's site path; links and
/// form actions go under `served`'s base, but `next` stays a site path (the worker adds the base when it redirects).
pub fn thread(
    block_id: &str,
    world: &World,
    n: Id,
    doc: &str,
    url: &str,
    d: &Discussion,
    served: &Served,
) -> String {
    let page = block_id == PAGE_BLOCK;
    let mut list: String =
        comments_of(world, n).iter().map(|c| item(c, block_id, d, served)).collect();
    if list.is_empty() && !page {
        list = "<p class=\"c-empty\">No comments yet.</p>".into();
    }
    let attrs = if page {
        " class=\"c-thread c-page\""
    } else {
        " class=\"c-thread\" role=\"region\" aria-label=\"Comments\""
    };
    format!(
        "<div{attrs} id=\"block-{}\"><div class=\"c-list\">{list}</div><div class=\"c-form\">{}</div></div>",
        esc(block_id),
        form(block_id, doc, url, d, served)
    )
}

fn form(block_id: &str, doc: &str, url: &str, d: &Discussion, served: &Served) -> String {
    let err = match &d.error {
        Some(e) if e.block == block_id => e.message.as_str(),
        _ => "",
    };
    let error = format!(
        "<p class=\"error\" role=\"alert\"{}>{}</p>",
        if err.is_empty() { " hidden" } else { "" },
        esc(err)
    );
    let Some(v) = &d.viewer else {
        if !d.sign_in {
            return "<p class=\"c-empty\">Comments are not set up yet.</p>".into();
        }
        let next = format!("{url}#block-{block_id}");
        return format!(
            "{}<p class=\"c-empty\"><a href=\"{}?next={}\">Sign in with GitHub</a> to comment.</p>",
            if err.is_empty() { "" } else { &error },
            esc(&served.href("/auth/github")),
            esc(&js::encode_uri_component(&next))
        );
    };
    format!(
        "<form class=\"c-compose\" method=\"post\" action=\"{action}\"><input type=\"hidden\" name=\"doc\" value=\"{doc}\" /><input type=\"hidden\" name=\"block\" value=\"{block}\" /><textarea name=\"body\" rows=\"3\" maxlength=\"{BODY_MAX}\" required placeholder=\"{ph}\" aria-label=\"Comment\"></textarea>{error}<div class=\"row\"><span>Signed in as {login}{admin} · <button type=\"submit\" form=\"c-signout\" class=\"c-signout\">sign out</button></span><button type=\"submit\">Post</button></div></form>",
        action = esc(&served.href("/api/comments")),
        doc = esc(doc),
        block = esc(block_id),
        ph = if block_id == PAGE_BLOCK { "Comment on the page" } else { "Comment on this" },
        login = esc(&v.login),
        admin = if v.admin { " (admin)" } else { "" },
    )
}

/// The sign-out form the threads' "sign out" buttons submit (forms cannot nest).
pub fn sign_out_form(url: &str, d: &Discussion, served: &Served) -> String {
    match d.viewer {
        Some(_) => format!(
            "<form id=\"c-signout\" method=\"post\" action=\"{}\" hidden><input type=\"hidden\" name=\"next\" value=\"{}\" /></form>",
            esc(&served.href("/auth/logout")),
            esc(url)
        ),
        None => String::new(),
    }
}
