#[derive(Debug, Clone)]
pub struct Profile {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub file_names: &'static [&'static str],
    pub atomic_nodes: &'static [&'static str],
    pub commutative_parents: &'static [&'static str],
    pub comment_nodes: &'static [&'static str],
}

impl Profile {
    #[must_use]
    pub fn is_atomic(&self, kind: &str) -> bool {
        self.atomic_nodes.contains(&kind)
    }

    #[must_use]
    pub fn is_commutative(&self, kind: &str) -> bool {
        self.commutative_parents.contains(&kind)
    }

    #[must_use]
    pub fn is_comment(&self, kind: &str) -> bool {
        self.comment_nodes.contains(&kind)
    }
}
