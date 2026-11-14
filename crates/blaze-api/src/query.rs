//! A rules query ([`crate::Request::Query`]) and what it found ([`crate::Response::Rows`]):
//! astlog's rules over the view's front end (`blaze_astlog`, docs/v2/plan/design/astlog-sema.md),
//! every relation they derive and every lint's finding, as data. A query writes nothing.
//!
//! Fields are declared in their keys' order (shorter first, then bytewise), as every type of
//! the API is.

use serde::{Deserialize, Serialize};

/// One value of a derived row.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Cell {
    /// Code: its range in its view file (`start..end`), its 1-based line, the file, its text.
    /// A node of a macro's expansion whose text is the call's input is the input's range.
    Code { end: u32, line: Option<u32>, path: String, text: String, start: u32 },
    /// A node a macro made, which no file holds: the macro call's `path:line`, and its text.
    Expanded { call: String, text: String },
    /// An item, by its def path from its crate's name (`std::sync::Mutex`).
    Def { path: String },
    /// A type, as the front end prints it.
    Type { text: String },
    Text(String),
    Int(i64),
}

/// One relation a query derived: its columns, and its rows in [`Cell`] order.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Relation {
    pub name: String,
    pub rows: Vec<Vec<Cell>>,
    pub columns: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
}

/// One row of a `(lint ...)`'s relation, its message rendered.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Finding {
    /// The row's first code cell.
    pub at: Option<Cell>,
    pub message: String,
    pub relation: String,
    pub severity: Severity,
}

/// What a query found: every relation by name, and every finding by place.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Rows {
    pub findings: Vec<Finding>,
    pub relations: Vec<Relation>,
}
