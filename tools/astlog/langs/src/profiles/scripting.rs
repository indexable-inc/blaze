use crate::types::Profile;

pub(crate) static PYTHON: Profile = Profile {
    name: "Python",
    extensions: &["py", "pyi"],
    file_names: &[],
    atomic_nodes: &["import_statement", "import_from_statement", "future_import_statement"],
    commutative_parents: &["dictionary", "set", "argument_list", "decorated_definition"],
    comment_nodes: &["comment"],
};
