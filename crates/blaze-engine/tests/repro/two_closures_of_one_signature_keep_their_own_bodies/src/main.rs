fn main() {
    let text: Result<&str, String> = Ok(std::hint::black_box("a\nb"));
    let first = text
        .and_then(|it| Ok(it.split_once('\n').ok_or("no newline")?.0))
        .and_then(|it| if it.len() == 1 { Ok(it) } else { Err(format!("{} bytes", it.len())) });
    println!("{first:?}");
    // std's `DebugList::finish` and `DebugTuple::finish` each give `and_then` a closure of one
    // signature: libm's build script printed a list with the tuple's `,)` and faulted.
    println!("{:?} {:?}", vec![String::from("x")], (String::from("y"),));
}
