//! [`value!`]: a [`Value`](crate::Value) from JSON-like syntax.

/// A [`Value`](crate::Value) from JSON-like syntax, the allocator first:
///
/// ```ignore
/// let v = value!(in Global; {
///     "ph": "X",
///     "pid": part.pid,
///     "args": [1, 2.5, null, true, name.as_str()],
///     (key): if on { "yes" } else { "no" },
///     "nested": {"a": {}},
/// });
/// ```
///
/// What it accepts, from today's `json!` call sites:
/// - `null`, `true`, `false`, `[...]` and `{...}` as themselves, nested to any depth;
/// - any other expression through [`ToValue`](crate::ToValue), by reference, so a value used
///   is not moved: numbers, `&str`, `String`s, `Option`s (`None` is null), slices, arrays,
///   `Vec`s, `Value`s and `Map`s (copied into the allocator);
/// - a key that is one token: a literal (`"a"`), a variable, or a parenthesised expression
///   (`(format_in!(a, "{x}"))`), anything with `AsRef<str>`;
/// - commas between, and one trailing comma.
///
/// A later key replaces an earlier one, keeping the first one's place. An element that starts
/// with `[` or `{` is read as a list or a map, so `[a, b].len()` must be parenthesised; so is
/// an expression that starts with `null`, `true` or `false` (`(true && x)`). The allocator
/// expression is evaluated once and cloned into each owner.
#[macro_export]
macro_rules! value {
    (in $alloc:expr; $($tt:tt)+) => {{
        let alloc = $alloc;
        $crate::__value!(alloc; $($tt)+)
    }};
}

#[macro_export]
#[doc(hidden)]
macro_rules! __value {
    // A list's elements, munched one at a time into `[$($done,)*]`.
    (@list $a:ident [$($done:expr,)*]) => {
        $crate::Value::List($crate::__private::Vec::from_array_in(
            [$($done),*],
            ::core::clone::Clone::clone(&$a),
        ))
    };
    (@list $a:ident [$($done:expr,)*] null $($rest:tt)*) => {
        $crate::__value!(@list_next $a [$($done,)* $crate::Value::Null,] $($rest)*)
    };
    (@list $a:ident [$($done:expr,)*] true $($rest:tt)*) => {
        $crate::__value!(@list_next $a [$($done,)* $crate::Value::Bool(true),] $($rest)*)
    };
    (@list $a:ident [$($done:expr,)*] false $($rest:tt)*) => {
        $crate::__value!(@list_next $a [$($done,)* $crate::Value::Bool(false),] $($rest)*)
    };
    (@list $a:ident [$($done:expr,)*] [$($inner:tt)*] $($rest:tt)*) => {
        $crate::__value!(@list_next $a [$($done,)* $crate::__value!($a; [$($inner)*]),] $($rest)*)
    };
    (@list $a:ident [$($done:expr,)*] {$($inner:tt)*} $($rest:tt)*) => {
        $crate::__value!(@list_next $a [$($done,)* $crate::__value!($a; {$($inner)*}),] $($rest)*)
    };
    (@list $a:ident [$($done:expr,)*] $next:expr, $($rest:tt)*) => {
        $crate::__value!(@list $a [$($done,)* $crate::__value!($a; $next),] $($rest)*)
    };
    (@list $a:ident [$($done:expr,)*] $last:expr) => {
        $crate::__value!(@list $a [$($done,)* $crate::__value!($a; $last),])
    };
    (@list_next $a:ident [$($done:expr,)*]) => {
        $crate::__value!(@list $a [$($done,)*])
    };
    (@list_next $a:ident [$($done:expr,)*] , $($rest:tt)*) => {
        $crate::__value!(@list $a [$($done,)*] $($rest)*)
    };

    // A map's entries, inserted one at a time into `$m`.
    (@map $a:ident $m:ident) => {};
    (@map $a:ident $m:ident $k:tt : null $($rest:tt)*) => {
        $crate::__private::insert(&mut $m, &$k, $crate::Value::Null, &$a);
        $crate::__value!(@map_next $a $m $($rest)*);
    };
    (@map $a:ident $m:ident $k:tt : true $($rest:tt)*) => {
        $crate::__private::insert(&mut $m, &$k, $crate::Value::Bool(true), &$a);
        $crate::__value!(@map_next $a $m $($rest)*);
    };
    (@map $a:ident $m:ident $k:tt : false $($rest:tt)*) => {
        $crate::__private::insert(&mut $m, &$k, $crate::Value::Bool(false), &$a);
        $crate::__value!(@map_next $a $m $($rest)*);
    };
    (@map $a:ident $m:ident $k:tt : [$($inner:tt)*] $($rest:tt)*) => {
        $crate::__private::insert(&mut $m, &$k, $crate::__value!($a; [$($inner)*]), &$a);
        $crate::__value!(@map_next $a $m $($rest)*);
    };
    (@map $a:ident $m:ident $k:tt : {$($inner:tt)*} $($rest:tt)*) => {
        $crate::__private::insert(&mut $m, &$k, $crate::__value!($a; {$($inner)*}), &$a);
        $crate::__value!(@map_next $a $m $($rest)*);
    };
    (@map $a:ident $m:ident $k:tt : $v:expr, $($rest:tt)*) => {
        $crate::__private::insert(&mut $m, &$k, $crate::__value!($a; $v), &$a);
        $crate::__value!(@map $a $m $($rest)*);
    };
    (@map $a:ident $m:ident $k:tt : $v:expr) => {
        $crate::__private::insert(&mut $m, &$k, $crate::__value!($a; $v), &$a);
    };
    (@map_next $a:ident $m:ident) => {};
    (@map_next $a:ident $m:ident , $($rest:tt)*) => {
        $crate::__value!(@map $a $m $($rest)*);
    };

    // One value.
    // A scalar names no allocator, so it is tied to `$a`'s here: `value!(in a; null)` is a
    // `Value<A>` of the allocator given, never one left to inference.
    ($a:ident; null) => {
        $crate::__private::of(&$a, $crate::Value::Null)
    };
    ($a:ident; true) => {
        $crate::__private::of(&$a, $crate::Value::Bool(true))
    };
    ($a:ident; false) => {
        $crate::__private::of(&$a, $crate::Value::Bool(false))
    };
    ($a:ident; [$($inner:tt)*]) => {
        $crate::__value!(@list $a [] $($inner)*)
    };
    ($a:ident; {}) => {
        $crate::Value::Map($crate::Map::new_in(::core::clone::Clone::clone(&$a)))
    };
    ($a:ident; {$($inner:tt)+}) => {{
        let mut map = $crate::Map::new_in(::core::clone::Clone::clone(&$a));
        $crate::__value!(@map $a map $($inner)*);
        $crate::Value::Map(map)
    }};
    ($a:ident; $other:expr) => {
        $crate::ToValue::to_value(&$other, ::core::clone::Clone::clone(&$a))
    };
}

/// [`FromValue`](crate::FromValue) and [`ToValue`](crate::ToValue) for a plain struct, a map
/// with one key per field named as the field: the required fields, then after `;` the
/// optional ones (each an `Option`, missing or null read as `None`, `None` left out when
/// written):
///
/// ```ignore
/// struct Run<A: Allocator> { suite: String<A>, timeout: u32, ids: Option<Vec<String<A>, A>> }
/// blaze_data::fields!(for<A> Run<A> { suite, timeout; ids });
/// struct Flags { on: bool }
/// blaze_data::fields!(for<A> Flags { on });
/// ```
///
/// The read is [`Cursor::fields`](crate::Cursor::fields), so a key that is no field is refused,
/// naming it and the fields.
#[macro_export]
macro_rules! fields {
    (for<$a:ident> $name:ident $(<$p:ident>)? { $($req:ident),* $(,)? $(; $($opt:ident),* $(,)?)? }) => {
        impl<$a: ::core::alloc::Allocator + ::core::clone::Clone> $crate::FromValue<$a> for $name $(<$p>)? {
            fn from_value(c: &$crate::Cursor<'_, '_, $a>) -> ::core::result::Result<Self, $crate::ShapeError<$a>> {
                c.fields(|f| {
                    ::core::result::Result::Ok($name {
                        $($req: f.take(::core::stringify!($req))?,)*
                        $($($opt: f.take_opt(::core::stringify!($opt))?,)*)?
                    })
                })
            }
        }

        impl<$a: ::core::alloc::Allocator + ::core::clone::Clone> $crate::ToValue<$a> for $name $(<$p>)? {
            fn to_value(&self, alloc: $a) -> $crate::Value<$a> {
                let mut map = $crate::Map::new_in(::core::clone::Clone::clone(&alloc));
                $(
                    map.insert(
                        $crate::__private::String::from_str_in(::core::stringify!($req), ::core::clone::Clone::clone(&alloc)),
                        $crate::ToValue::to_value(&self.$req, ::core::clone::Clone::clone(&alloc)),
                    );
                )*
                $($(
                    if let ::core::option::Option::Some(value) = &self.$opt {
                        map.insert(
                            $crate::__private::String::from_str_in(::core::stringify!($opt), ::core::clone::Clone::clone(&alloc)),
                            $crate::ToValue::to_value(value, ::core::clone::Clone::clone(&alloc)),
                        );
                    }
                )*)?
                $crate::Value::Map(map)
            }
        }
    };
}
