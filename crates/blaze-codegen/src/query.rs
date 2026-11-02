//! Codegen's query kinds, in [`blaze_query::kinds::BACK`]'s first half (link's is the second,
//! `0x4800..0x5000`), and the store keys of what it keeps.
//!
//! Every key is made from exactly what the value is a function of:
//!
//! | value | key | a function of |
//! |---|---|---|
//! | a dev template | [`keys::template`] | the instance key: code hash, type-argument facts, settings (the policy among them) |
//! | a release template | [`keys::release`] | [`crate::summary::Imports::blob_key`] of [`crate::backend::laid_out`]: the instance key, which of its and its imports' references are one target, and everything imported and assumed |
//! | a summary | [`keys::summary`] | the instance key, what it builds in and its callees' keys, which the instance key takes by their interface (`driver::summarized`) |
//! | a masked shape | [`keys::masked`] | the instance key; read and put by no build, as each fold candidate is masked from its IR in the build that folds it (`driver::dev`) |
//! | a folded body | [`keys::folded`] | the group's key: masked shape, settings, which constants vary |
//! | a thunk | [`keys::thunk`] | the group's key and the member's instance key, which holds its constants |
//! | a static's template | [`keys::static_value`] | the value's content (with its layout) and the settings |
//! | a side table | [`keys::side`] | its bytes |

use blaze_query::QueryKind;

pub const TEMPLATE: QueryKind = QueryKind(0x4000);
pub const RELEASE: QueryKind = QueryKind(0x4001);
pub const SUMMARY: QueryKind = QueryKind(0x4002);
pub const MASKED: QueryKind = QueryKind(0x4003);
pub const FOLDED: QueryKind = QueryKind(0x4004);
pub const STATIC: QueryKind = QueryKind(0x4005);
/// The whole build's import decisions, from every summary.
pub const IMPORTS: QueryKind = QueryKind(0x4006);

pub mod keys {
    use blaze_cas::Key;
    use blaze_ids::{ContentId, Hasher};

    use crate::{InstanceKey, Settings};

    fn key(domain: &'static str, parts: &[&ContentId]) -> Key {
        let mut hasher = Hasher::new(domain);
        for part in parts {
            hasher.id(part);
        }
        Key(hasher.finish())
    }

    pub fn template(instance: &InstanceKey) -> Key {
        key("blaze v2 store template 2", &[&instance.0])
    }

    pub fn release(blob: &ContentId) -> Key {
        key("blaze v2 store release template 2", &[blob])
    }

    pub fn summary(instance: &InstanceKey) -> Key {
        key("blaze v2 store summary 1", &[&instance.0])
    }

    pub fn masked(instance: &InstanceKey) -> Key {
        key("blaze v2 store masked 1", &[&instance.0])
    }

    pub fn folded(group: &InstanceKey) -> Key {
        key("blaze v2 store folded 2", &[&group.0])
    }

    pub fn thunk(group: &InstanceKey, member: &InstanceKey) -> Key {
        key("blaze v2 store thunk 2", &[&group.0, &member.0])
    }

    pub fn static_value(value: &ContentId, settings: &Settings) -> Key {
        key("blaze v2 store static 2", &[value, &settings.0])
    }

    pub fn side(id: &ContentId) -> Key {
        key("blaze v2 store side 1", &[id])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn one_id_is_another_key_in_each_kind() {
            let id = ContentId([1; 32]);
            let instance = InstanceKey(id);
            let keys = [
                template(&instance),
                release(&id),
                summary(&instance),
                masked(&instance),
                folded(&instance),
                thunk(&instance, &instance),
                static_value(&id, &Settings(id)),
                side(&id),
            ];
            let unique: std::collections::BTreeSet<_> = keys.iter().collect();
            assert_eq!(unique.len(), keys.len());
        }
    }
}
