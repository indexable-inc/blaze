//! Found in objc2 0.6's `define_class!` (accesskit_macos' `frame`; #1259): the `extern "C"`
//! function the macro makes for a method returns `<CGRect as ConvertReturn<()>>::Inner`, which
//! is `CGRect`. The C ABI's classifiers matched on the type's kind before normalizing it, so
//! they refused the projection (`Alias(..) in the C ABI`).

pub trait ConvertReturn<M> {
    type Inner;
    fn into_inner(self) -> Self::Inner;
}

impl<T> ConvertReturn<()> for T {
    type Inner = T;
    fn into_inner(self) -> T {
        self
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub origin: Point,
    pub size: Point,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Mixed {
    pub id: u32,
    pub scale: f32,
}

/// A field that is a projection too.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Held<R: ConvertReturn<()>>
where
    <R as ConvertReturn<()>>::Inner: Copy,
{
    pub inner: <R as ConvertReturn<()>>::Inner,
    pub tag: u32,
}

extern "C" fn method<R: ConvertReturn<()>>(make: fn() -> R) -> <R as ConvertReturn<()>>::Inner {
    make().into_inner()
}

extern "C" fn echo<R: ConvertReturn<()>>(
    value: <R as ConvertReturn<()>>::Inner,
) -> <R as ConvertReturn<()>>::Inner {
    value
}

extern "C" fn held<R: ConvertReturn<()>>(value: Held<R>) -> Held<R>
where
    <R as ConvertReturn<()>>::Inner: Copy,
{
    Held { tag: value.tag + 1, ..value }
}

fn rect() -> Rect {
    Rect { origin: Point { x: 1.0, y: 2.0 }, size: Point { x: 3.0, y: 4.0 } }
}

fn point() -> Point {
    Point { x: 5.5, y: -6.5 }
}

fn mixed() -> Mixed {
    Mixed { id: 7, scale: 0.25 }
}

pub fn run() -> (Rect, Point, Mixed, Rect, Held<Point>, Held<Mixed>) {
    use std::hint::black_box;
    let f: extern "C" fn(fn() -> Rect) -> Rect = black_box(method::<Rect>);
    let g: extern "C" fn(fn() -> Point) -> Point = black_box(method::<Point>);
    let h: extern "C" fn(fn() -> Mixed) -> Mixed = black_box(method::<Mixed>);
    let e: extern "C" fn(Rect) -> Rect = black_box(echo::<Rect>);
    let p: extern "C" fn(Held<Point>) -> Held<Point> = black_box(held::<Point>);
    let m: extern "C" fn(Held<Mixed>) -> Held<Mixed> = black_box(held::<Mixed>);
    (
        f(rect),
        g(point),
        h(mixed),
        e(rect()),
        p(Held { inner: point(), tag: 1 }),
        m(Held { inner: mixed(), tag: 2 }),
    )
}
