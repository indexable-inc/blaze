use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::ptr;
use std::sync::OnceLock;

use blaze_ids::{ContentId, Domain, Interner, Sharded};

/// Every string interned, by its content ID: a hit is lock-free probes and no allocation.
static STRINGS: OnceLock<Sharded<Box<str>>> = OnceLock::new();
static STRING: Domain = Domain::new("blaze-source interned string 1");

/// A string kept once for the life of the process, so that names compare by pointer and copy
/// for free. Package, feature and dependency names are what the resolver compares most.
#[derive(Clone, Copy)]
pub struct InternedString {
    inner: &'static str,
}

impl serde::Serialize for InternedString {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.inner)
    }
}

impl InternedString {
    pub fn new(s: &str) -> InternedString {
        let strings = STRINGS.get_or_init(Sharded::new);
        let id = ContentId::of_domain(&STRING, s.as_bytes());
        // alloc: once per distinct string for the life of the process, as cargo's interner.
        let at = strings.intern(id, || s.into());
        InternedString { inner: strings.get(at) }
    }

    pub fn as_str(&self) -> &'static str {
        self.inner
    }
}

impl<'a> From<&'a str> for InternedString {
    fn from(s: &'a str) -> Self {
        InternedString::new(s)
    }
}

impl From<&String> for InternedString {
    fn from(s: &String) -> Self {
        InternedString::new(s)
    }
}

impl From<String> for InternedString {
    fn from(s: String) -> Self {
        InternedString::new(&s)
    }
}

impl PartialEq for InternedString {
    fn eq(&self, other: &InternedString) -> bool {
        ptr::eq(self.inner, other.inner)
    }
}

impl Eq for InternedString {}

impl PartialEq<str> for InternedString {
    fn eq(&self, other: &str) -> bool {
        self.inner == other
    }
}

impl PartialEq<&str> for InternedString {
    fn eq(&self, other: &&str) -> bool {
        self.inner == *other
    }
}

impl Hash for InternedString {
    // Hashed as the string so that a `HashMap<InternedString, _>` can be read with a `&str`.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.hash(state);
    }
}

impl Ord for InternedString {
    fn cmp(&self, other: &InternedString) -> Ordering {
        self.inner.cmp(other.inner)
    }
}

impl PartialOrd for InternedString {
    fn partial_cmp(&self, other: &InternedString) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Deref for InternedString {
    type Target = str;

    fn deref(&self) -> &'static str {
        self.inner
    }
}

impl AsRef<str> for InternedString {
    fn as_ref(&self) -> &str {
        self.inner
    }
}

impl Borrow<str> for InternedString {
    fn borrow(&self) -> &str {
        self.inner
    }
}

impl fmt::Debug for InternedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.inner, f)
    }
}

impl fmt::Display for InternedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.inner, f)
    }
}

#[cfg(test)]
mod tests {
    use super::InternedString;
    use blaze_hash::HashMap;

    #[test]
    fn same_string_same_pointer() {
        let a = InternedString::new("serde");
        let b = InternedString::from(String::from("serde"));
        assert!(std::ptr::eq(a.as_str(), b.as_str()));
        assert_eq!(a, b);
        assert_ne!(a, InternedString::new("serde_json"));
    }

    #[test]
    fn ordered_and_looked_up_as_str() {
        assert!(InternedString::new("a") < InternedString::new("b"));
        let mut map = HashMap::default();
        map.insert(InternedString::new("default"), 1);
        assert_eq!(map.get("default"), Some(&1));
    }

    #[test]
    fn racing_threads_get_one_string() {
        let names: Vec<String> = (0..64).map(|it| format!("racing-{it}")).collect();
        let interned: Vec<Vec<&'static str>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| names.iter().map(|it| InternedString::new(it).as_str()).collect())
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        for each in &interned[1..] {
            for (a, b) in interned[0].iter().zip(each) {
                assert!(std::ptr::eq(*a, *b), "{a} twice");
            }
        }
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn a_hit_allocates_nothing() {
        let first = InternedString::new("a-hit");
        let source = crate::SourceId::crates_io();
        let again = blaze_alloc_count::assert_at_most(0, "an interned string's hit", || {
            InternedString::new("a-hit")
        });
        assert_eq!(first, again);
        let again = blaze_alloc_count::assert_at_most(0, "crates.io's source id", crate::SourceId::crates_io);
        assert!(source.full_eq(again));
    }
}
