use crate::types::Profile;

pub(crate) static TOML: Profile = Profile {
    name: "TOML",
    extensions: &["toml"],
    file_names: &["Cargo.toml", "pyproject.toml"],
    atomic_nodes: &[],
    commutative_parents: &["table", "inline_table"],
    comment_nodes: &["comment"],
};
