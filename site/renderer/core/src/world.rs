//! The world the renderer reads: the site's entities as Flecs has them (name, parent, ordered children, tags, pairs,
//! component values and prefabs), loaded from a snapshot (`snapshot.rs`). Comments are entities too, added for one
//! render and removed after it (`World::mark`, `World::rollback`).
//!
//! The shape is runtime data, the same a host sees in the snapshot: component values are maps of scalars keyed by the
//! component's path, so nothing here is generic over the schema.

use std::collections::{BTreeMap, HashMap};

/// An entity, by its index in the world. Stable while the entity lives.
pub type Id = usize;

/// One field of a component value.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

/// A component value: its fields by name (`P` is `{text}`, `Code` is `{lang, text}`).
pub type Value = BTreeMap<String, Scalar>;

#[derive(Clone, Debug, Default)]
pub struct Entity {
    pub name: String,
    /// Full path, dotted: `site.rfcs.capabilities.motivation.why`.
    pub path: String,
    pub parent: Option<Id>,
    /// In Flecs's order: declared order under OrderedChildren.
    pub children: Vec<Id>,
    /// Tags, own and inherited, with `flecs.core.` dropped (`OrderedChildren`, `Discussable`).
    pub tags: Vec<String>,
    /// Relationship to targets: `Status` to `["Draft"]`, `Cites` to `["refs.zig_016", ...]`.
    pub pairs: Vec<(String, Vec<String>)>,
    /// Component values, own and inherited (the nearest wins), by component path.
    pub values: Vec<(String, Value)>,
    /// Every prefab this entity is an instance of, nearest first: `["Rfc", "Doc", "Page"]`.
    pub isa: Vec<String>,
}

impl Entity {
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    pub fn value(&self, component: &str) -> Option<&Value> {
        self.values.iter().find(|(k, _)| k == component).map(|(_, v)| v)
    }

    pub fn has(&self, component: &str) -> bool {
        self.value(component).is_some()
    }

    pub fn targets(&self, rel: &str) -> Option<&[String]> {
        self.pairs.iter().find(|(r, _)| r == rel).map(|(_, t)| t.as_slice())
    }

    pub fn is_a(&self, prefab: &str) -> bool {
        self.isa.iter().any(|p| p == prefab)
    }
}

#[derive(Clone, Debug, Default)]
pub struct World {
    entities: Vec<Entity>,
    by_path: HashMap<String, Id>,
}

/// What `World::mark` returns: the world's size at that point.
#[derive(Clone, Copy, Debug)]
pub struct Mark(usize);

impl World {
    pub fn get(&self, id: Id) -> &Entity {
        &self.entities[id]
    }

    pub fn lookup(&self, path: &str) -> Option<Id> {
        self.by_path.get(path).copied()
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// The `Rfc` children of `parent`, by `Number` (then path, so ties are stable). Child order is not used:
    /// across files it follows creation order, which depends on file names and forward references.
    pub fn rfcs(&self, parent: Id) -> Vec<Id> {
        let num = |e: &Entity| crate::js::to_number(e.value("Number").and_then(|v| v.get("value")));
        let mut v: Vec<Id> = self
            .get(parent)
            .children
            .iter()
            .copied()
            .filter(|&c| self.get(c).is_a("Rfc"))
            .collect();
        v.sort_by(|&a, &b| {
            let (x, y) = (self.get(a), self.get(b));
            num(x).total_cmp(&num(y)).then_with(|| x.path.cmp(&y.path))
        });
        v
    }

    /// The lesson pages under `parent` (pages with `Lesson`), by `Lesson.order`, then path.
    pub fn lessons(&self, parent: Id) -> Vec<Id> {
        let order =
            |e: &Entity| crate::js::to_number(e.value("Lesson").and_then(|v| v.get("order")));
        let mut v: Vec<Id> = self
            .get(parent)
            .children
            .iter()
            .copied()
            .filter(|&c| self.get(c).is_a("Page") && self.get(c).has("Lesson"))
            .collect();
        v.sort_by(|&a, &b| {
            let (x, y) = (self.get(a), self.get(b));
            order(x).total_cmp(&order(y)).then_with(|| x.path.cmp(&y.path))
        });
        v
    }

    /// The pages a `Course` widget's lessons and updates are under: its `(Lessons, <page>)` targets, or the page
    /// holding it.
    pub fn course_sources(&self, course: Id) -> Result<Vec<Id>, String> {
        let e = self.get(course);
        match e.targets("Lessons") {
            Some(paths) if !paths.is_empty() => paths
                .iter()
                .map(|path| {
                    self.lookup(path)
                        .ok_or_else(|| format!("{}: (Lessons, {path}) names nothing", e.path))
                })
                .collect(),
            _ => Ok(vec![e.parent.ok_or_else(|| format!("{}: Course has no parent", e.path))?]),
        }
    }

    /// The `Course` widget on a page (not looking into the page's child pages), if it has one.
    pub fn course_in(&self, page: Id) -> Option<Id> {
        self.tagged_in(page, "Course")
    }

    /// The first block on a page with the tag `tag` (not looking into the page's child pages), if any.
    pub fn tagged_in(&self, page: Id, tag: &str) -> Option<Id> {
        let mut todo: Vec<Id> = self.get(page).children.clone();
        while let Some(n) = todo.pop() {
            let e = self.get(n);
            if e.is_a("Page") {
                continue;
            }
            if e.has_tag(tag) {
                return Some(n);
            }
            todo.extend(e.children.iter().copied());
        }
        None
    }

    /// The course whose lessons or updates are under `parent`: the page holding its `Course` widget, and the graph.
    pub fn course_of(
        &self,
        pages: &[crate::render::Page],
        parent: Id,
    ) -> Result<Option<(usize, crate::course::Course)>, String> {
        for (i, p) in pages.iter().enumerate() {
            if let Some(c) = self.course_in(p.node) {
                let sources = self.course_sources(c)?;
                if sources.contains(&parent) {
                    return Ok(Some((i, crate::course::build(self, &sources)?)));
                }
            }
        }
        Ok(None)
    }

    /// Every entity, parents before children.
    pub fn iter(&self) -> impl Iterator<Item = (Id, &Entity)> {
        self.entities.iter().enumerate()
    }

    /// Adds an entity as the last child of `parent` (or as a root). `name` must be new under the parent.
    pub fn spawn(&mut self, parent: Option<Id>, mut e: Entity) -> Result<Id, String> {
        e.path = match parent {
            Some(p) => format!("{}.{}", self.entities[p].path, e.name),
            None => e.name.clone(),
        };
        if self.by_path.contains_key(&e.path) {
            return Err(format!("{} exists already", e.path));
        }
        e.parent = parent;
        e.children.clear();
        let id = self.entities.len();
        self.by_path.insert(e.path.clone(), id);
        self.entities.push(e);
        if let Some(p) = parent {
            self.entities[p].children.push(id);
        }
        Ok(id)
    }

    pub fn mark(&self) -> Mark {
        Mark(self.entities.len())
    }

    /// Removes every entity spawned since `mark`, newest first.
    pub fn rollback(&mut self, mark: Mark) {
        while self.entities.len() > mark.0 {
            let e = self.entities.pop().expect("len > mark");
            self.by_path.remove(&e.path);
            if let Some(p) = e.parent
                && p < self.entities.len()
            {
                let popped = self.entities[p].children.pop();
                debug_assert_eq!(popped, Some(self.entities.len()));
            }
        }
    }
}
