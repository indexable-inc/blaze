fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, "smoke-edition: bstd built under blaze")?;
    Ok(())
}
