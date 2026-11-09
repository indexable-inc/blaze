//! Environment variables (docs/v2/plan/design/std.md, `Env`): the world's, read once when it
//! was made (handed in by the entry until `bstd::rt` reads the start block), and narrowed to
//! named ones by [`Env::only`], which never widens.

use std::sync::Arc;

use crate::Plant;

/// The variables a holder may read.
#[derive(Clone)]
pub struct Env {
    vars: Arc<[(String, String)]>,
    /// The names it may read; `None` for every one.
    allowed: Option<Arc<[String]>>,
    plant: Plant,
}

impl Env {
    pub(crate) fn new(vars: Arc<[(String, String)]>, plant: Plant) -> Env {
        Env { vars, allowed: None, plant }
    }

    /// The variable `name`, if it is set and this may read it.
    pub fn get(&self, name: &str) -> Option<&str> {
        if !self.may_read(name) {
            return None;
        }
        self.vars.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str())
    }

    /// Whether this may read `name`, set or not.
    pub fn may_read(&self, name: &str) -> bool {
        self.allowed.as_deref().is_none_or(|allowed| allowed.iter().any(|it| it == name))
    }

    /// Only the variables of `names` this may read already: never one more.
    pub fn only(&self, names: &[&str]) -> Env {
        let kept = names.iter().filter(|name| match self.plant {
            Plant::EnvOnlyReplaces => true,
            _ => self.may_read(name),
        });
        // alloc: once per narrowing, the names kept.
        let allowed: Arc<[String]> = kept.map(|name| (*name).to_owned()).collect();
        Env { vars: Arc::clone(&self.vars), allowed: Some(allowed), plant: self.plant }
    }

    /// Each variable this may read that is set, in the world's order.
    pub fn each(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars
            .iter()
            .filter(|(name, _)| self.may_read(name))
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.allowed {
            None => f.write_str("Env(all)"),
            Some(allowed) => write!(f, "Env(only {allowed:?})"),
        }
    }
}
