//@ expect: accept
trait Speak {
    fn speak(&self) -> String;

    fn twice(&self) -> String {
        format!("{0} {0}", self.speak())
    }
}

struct Dog;
struct Cat;

impl Speak for Dog {
    fn speak(&self) -> String {
        "woof".to_owned()
    }
}

impl Speak for Cat {
    fn speak(&self) -> String {
        "meow".to_owned()
    }
}

fn main() {
    let animals: Vec<Box<dyn Speak>> = vec![Box::new(Dog), Box::new(Cat)];
    for it in &animals {
        println!("{}", it.twice());
    }
}
