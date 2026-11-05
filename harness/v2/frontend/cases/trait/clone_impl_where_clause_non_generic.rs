//@ expect: accept
//@ edition: 2021
// A Clone impl on a non-generic struct with a global where clause that holds
// (`where String: Clone`): the shape table does not take impls with where clauses.
struct Tag(u8);

impl Clone for Tag
where
    String: Clone,
{
    fn clone(&self) -> Tag {
        Tag(self.0 + 1)
    }
}

fn main() {
    let t = Tag(1);
    let u = t.clone();
    let v = vec![Tag(5)].clone();
    println!("{} {} {}", t.0, u.0, v[0].0);
}
