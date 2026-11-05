struct Here { file: &'static str, line: u32 }

impl Here {
    fn show(&self) -> String {
        let file = if self.file.ends_with("main.rs") { "main.rs" } else { self.file };
        format!("{file}:{}", self.line)
    }
}

fn here() -> &'static Here {
    &Here { file: file!(), line: line!() }
}

fn main() {
    let far = here();
    println!("{}", Here { file: file!(), line: line!() }.show());
    println!("{}", far.show());
}
