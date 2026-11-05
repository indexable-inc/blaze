//! The macros of `shapes`, which blaze's const evaluator runs (docs/future/const-macros.md).

/// `impl Named for <the struct>`, whose `NAME` is the struct's name, read from its tokens as
/// text.
#[proc_macro_derive(Named)]
pub fn named(input: &str) -> String {
    let mut words = input.split_whitespace().skip_while(|it| *it != "struct");
    let name = words.nth(1).unwrap_or_default();
    format!("impl crate::Named for {name} {{ const NAME: &'static str = \"{name}\"; }}")
}

/// `impl Fields for <the struct>`, whose `FIELDS` are the names of its fields, read from the
/// item through `const_ast`'s views.
#[proc_macro_derive(Fields)]
pub fn fields(item: &const_ast::Item) -> String {
    let adt = item.as_struct().expect("`Fields` is derived for a struct");
    let name = adt.name().map_or("", |it| it.text());
    let mut fields = Vec::new();
    if let Some(const_ast::FieldList::RecordFieldList(list)) = adt.field_list() {
        for field in list.fields() {
            fields.push(format!("\"{}\"", field.name().map_or("", |it| it.text())));
        }
    }
    let fields = fields.join(", ");
    format!("impl crate::Fields for {name} {{ const FIELDS: &'static [&'static str] = &[{fields}]; }}")
}
