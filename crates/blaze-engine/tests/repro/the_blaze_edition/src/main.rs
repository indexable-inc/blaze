fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, "Hello, world!")?;
    Ok(())
}
