fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, "Hello, world!")?;
    Ok(())
}
