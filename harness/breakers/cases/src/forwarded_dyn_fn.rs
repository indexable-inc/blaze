//! ty forwards a reference to a `dyn Fn` as another trait object. Its vtable points to the
//! standard library implementation of `Fn::call` for `&F`.

type Callback<'a> = dyn Fn(&mut usize, [usize; 5]) -> [usize; 4] + 'a;

#[inline(never)]
fn invoke(callback: &Callback<'_>, value: &mut usize) -> [usize; 4] {
    callback(value, [2, 3, 5, 7, 11])
}

#[inline(never)]
fn forward(callback: &Callback<'_>, value: &mut usize) -> [usize; 4] {
    invoke(&callback, value)
}

pub fn run() -> ([usize; 4], usize) {
    let offset = 5;
    let mut value = 17;
    let result = forward(
        &|value, context| {
            *value += offset + context[4];
            [*value, context[0], context[1], context[2]]
        },
        &mut value,
    );
    (result, value)
}
