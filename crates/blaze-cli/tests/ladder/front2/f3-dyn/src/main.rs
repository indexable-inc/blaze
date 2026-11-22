// Own program f3-dyn (front2), edition blaze-2026.11, so `Box` and the unsizing lang items come
// from the real libraries: a call through `&dyn Area`, a call through `Box<dyn Area>` and that
// box's drop through the vtable's drop word. No formatting: the lines are chosen, not printed.
// The boxes are blaze-own's (the prelude's, D120), made in the frame `main` was handed
// (`cx.frame()`), so the free goes through the vtable's size and alignment to that frame.
trait Area {
    fn area(&self) -> u32;
}

struct Square(u32);

struct Counted<'a> {
    side: u32,
    drops: &'a mut u32,
}

impl Area for Square {
    fn area(&self) -> u32 {
        self.0 * self.0
    }
}

impl Area for Counted<'_> {
    fn area(&self) -> u32 {
        self.side * 2
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        *self.drops += 1;
    }
}

fn through_ref(a: &dyn Area) -> u32 {
    a.area()
}

fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    let mut drops = 0;
    let square = Square(5);
    // 25 + 2 * 3 + 2 * 4 + 3 = 42 when every call goes to its own impl.
    let mut total = through_ref(&square);
    {
        let first: Box<dyn Area + '_, &Frame> = Box::new_in(Counted { side: 3, drops: &mut drops }, cx.frame());
        total += first.area();
    }
    {
        let second: Box<dyn Area + '_, &Frame> = Box::new_in(Counted { side: 4, drops: &mut drops }, cx.frame());
        total += through_ref(&*second) + 3;
    }
    if total == 42 {
        out.line(cx, "dyn: 42")?;
    } else {
        out.line(cx, "dyn: wrong total")?;
    }
    if drops == 2 {
        out.line(cx, "dropped: 2")?;
    } else {
        out.line(cx, "dropped: wrong count")?;
    }
    Ok(())
}
