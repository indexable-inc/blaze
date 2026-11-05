//@ expect: accept
trait Label {
    fn label(&self) -> String {
        "default".to_owned()
    }
}

struct Plain;
struct Custom;

impl Label for Plain {}

impl Label for Custom {
    fn label(&self) -> String {
        "custom".to_owned()
    }
}

fn main() {
    println!("{} {}", Plain.label(), Custom.label());
}
