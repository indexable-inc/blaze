use crate::types::Profile;

pub(crate) static RUST: Profile = Profile {
    name: "Rust",
    extensions: &["rs"],
    file_names: &[],
    atomic_nodes: &["use_declaration", "attribute_item", "inner_attribute_item"],
    commutative_parents: &[
        "declaration_list",
        "use_list",
        "enum_variant_list",
        "field_declaration_list",
    ],
    comment_nodes: &["line_comment", "block_comment"],
};
