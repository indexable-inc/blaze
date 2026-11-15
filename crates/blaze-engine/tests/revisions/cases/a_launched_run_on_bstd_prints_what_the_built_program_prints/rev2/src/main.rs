fn value() -> u32 {
    (1..=10).sum::<u32>() + 1
}

fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, format_args!("hello: {}", value()))?;
    Ok(())
}
