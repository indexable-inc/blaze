//@ expect: accept
//@ edition: 2021
// An enum with data in some variants only: every variant's fields hold.
enum Message {
    Quit,
    Move { x: i32, y: i32 },
    Write(String),
    Color(u8, u8, u8),
    Nested(Option<Box<Message>>),
}

fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}

fn describe(m: &Message) -> String {
    match m {
        Message::Quit => String::from("quit"),
        Message::Move { x, y } => format!("move {x} {y}"),
        Message::Write(s) => format!("write {s}"),
        Message::Color(r, g, b) => format!("color {r} {g} {b}"),
        Message::Nested(n) => format!("nested {}", n.as_deref().map_or(String::from("-"), describe)),
    }
}

fn main() {
    let m = Message::Nested(Some(Box::new(Message::Move { x: 1, y: 2 })));
    println!("{} {}", is_send::<Message>() + is_sync::<Message>(), describe(&m));
    println!("{} {} {}", describe(&Message::Quit), describe(&Message::Write(String::from("w"))), describe(&Message::Color(1, 2, 3)));
}
