//! The macros, in the call shape crates already write:
//!
//! ```text
//! info_span!([target: <expr>,] [parent: <expr>,] "<name>" [, field]*)
//! info!([target: <expr>,] [parent: <expr>,] [field,]* ["<fmt>" [, <arg>]*])
//! field := ident | ident = <expr> | ident = %<expr> | ident = ?<expr> | %ident | ?ident
//! ```
//!
//! `%` gives a field by its `Display`, `?` by its `Debug`, and a plain value must be a
//! [`crate::ToValue`]. `field = field::Empty` declares a field a later `span.record(..)` gives.
//! A site's target is its module's path unless it names one. Each site is one `static`
//! [`crate::Metadata`] and one [`crate::Site`]; nothing is evaluated past the site's state's
//! branch ([`crate::Site::how`]) unless something records the site.

/// Reads a site's fields and message, then gives them to `$done` (`__span` or `__event`):
/// `[names] [values] [message]`.
#[doc(hidden)]
#[macro_export]
macro_rules! __fields {
    ($done:ident $site:tt [$($n:expr),*] [$($v:expr),*]) => {
        $crate::$done!(@site $site [$($n),*] [$($v),*] [])
    };
    ($done:ident $site:tt $ns:tt $vs:tt , $($rest:tt)*) => {
        $crate::__fields!($done $site $ns $vs $($rest)*)
    };
    ($done:ident $site:tt $ns:tt $vs:tt $message:literal $($args:tt)*) => {
        $crate::$done!(@site $site $ns $vs [$message $($args)*])
    };
    ($done:ident $site:tt [$($n:expr),*] [$($v:expr),*] $k:ident = % $e:expr $(, $($rest:tt)*)?) => {
        $crate::__fields!($done $site [$($n,)* stringify!($k)] [$($v,)* ::core::option::Option::Some($crate::Value::Display(&$e))] $($($rest)*)?)
    };
    ($done:ident $site:tt [$($n:expr),*] [$($v:expr),*] $k:ident = ? $e:expr $(, $($rest:tt)*)?) => {
        $crate::__fields!($done $site [$($n,)* stringify!($k)] [$($v,)* ::core::option::Option::Some($crate::Value::Debug(&$e))] $($($rest)*)?)
    };
    ($done:ident $site:tt [$($n:expr),*] [$($v:expr),*] $k:ident = $e:expr $(, $($rest:tt)*)?) => {
        $crate::__fields!($done $site [$($n,)* stringify!($k)] [$($v,)* $crate::ToValue::to_value(&$e)] $($($rest)*)?)
    };
    ($done:ident $site:tt [$($n:expr),*] [$($v:expr),*] % $k:ident $(, $($rest:tt)*)?) => {
        $crate::__fields!($done $site [$($n,)* stringify!($k)] [$($v,)* ::core::option::Option::Some($crate::Value::Display(&$k))] $($($rest)*)?)
    };
    ($done:ident $site:tt [$($n:expr),*] [$($v:expr),*] ? $k:ident $(, $($rest:tt)*)?) => {
        $crate::__fields!($done $site [$($n,)* stringify!($k)] [$($v,)* ::core::option::Option::Some($crate::Value::Debug(&$k))] $($($rest)*)?)
    };
    ($done:ident $site:tt [$($n:expr),*] [$($v:expr),*] $k:ident $(, $($rest:tt)*)?) => {
        $crate::__fields!($done $site [$($n,)* stringify!($k)] [$($v,)* $crate::ToValue::to_value(&$k)] $($($rest)*)?)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __span {
    (@site ($target:expr, $parent:expr, $level:expr, $name:literal) [$($n:expr),*] [$($v:expr),*] []) => {{
        static META: $crate::Metadata<'static> = $crate::Metadata::new(
            $name,
            $target,
            $level,
            ::core::option::Option::Some(file!()),
            ::core::option::Option::Some(line!()),
            ::core::option::Option::Some(module_path!()),
            $crate::field::FieldSet::new(&[$($n),*]),
            $crate::Kind::SPAN,
        );
        static SITE: $crate::Site = $crate::Site::new(&META);
        if $crate::Level::compiled($level)
            && let ::core::option::Option::Some(how) = SITE.how()
        {
            $crate::Span::open(&SITE, how, $parent, &[$($v),*])
        } else {
            $crate::Span::none()
        }
    }};
    (@site $site:tt $ns:tt $vs:tt [$($message:tt)+]) => {
        ::core::compile_error!("a span has a name and fields, no message")
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __event {
    (@site ($target:expr, $parent:expr, $level:expr) [$($n:expr),*] [$($v:expr),*] []) => {{
        static META: $crate::Metadata<'static> = $crate::Metadata::new(
            concat!("event ", file!(), ":", line!()),
            $target,
            $level,
            ::core::option::Option::Some(file!()),
            ::core::option::Option::Some(line!()),
            ::core::option::Option::Some(module_path!()),
            $crate::field::FieldSet::new(&[$($n),*]),
            $crate::Kind::EVENT,
        );
        static SITE: $crate::Site = $crate::Site::new(&META);
        if $crate::Level::compiled($level)
            && let ::core::option::Option::Some(how) = SITE.how()
        {
            $crate::Event::emit(&SITE, how, $parent, &[$($v),*]);
        }
    }};
    (@site ($target:expr, $parent:expr, $level:expr) [$($n:expr),*] [$($v:expr),*] [$($message:tt)+]) => {{
        static META: $crate::Metadata<'static> = $crate::Metadata::new(
            concat!("event ", file!(), ":", line!()),
            $target,
            $level,
            ::core::option::Option::Some(file!()),
            ::core::option::Option::Some(line!()),
            ::core::option::Option::Some(module_path!()),
            $crate::field::FieldSet::new(&[$($n,)* "message"]),
            $crate::Kind::EVENT,
        );
        static SITE: $crate::Site = $crate::Site::new(&META);
        if $crate::Level::compiled($level)
            && let ::core::option::Option::Some(how) = SITE.how()
        {
            $crate::Event::emit(
                &SITE,
                how,
                $parent,
                &[$($v,)* ::core::option::Option::Some($crate::Value::Display(&format_args!($($message)+)))],
            );
        }
    }};
}

/// A span of `level`: `span!(Level::INFO, "name", field = value)`, with `target:` and
/// `parent:` before the level as in the level macros.
#[macro_export]
macro_rules! span {
    (target: $target:expr, parent: $parent:expr, $level:expr, $name:literal $($rest:tt)*) => {
        $crate::__fields!(__span ($target, $crate::Parent::of($parent), $level, $name) [] [] $($rest)*)
    };
    (target: $target:expr, $level:expr, $name:literal $($rest:tt)*) => {
        $crate::__fields!(__span ($target, $crate::Parent::Current, $level, $name) [] [] $($rest)*)
    };
    (parent: $parent:expr, $level:expr, $name:literal $($rest:tt)*) => {
        $crate::__fields!(__span (module_path!(), $crate::Parent::of($parent), $level, $name) [] [] $($rest)*)
    };
    ($level:expr, $name:literal $($rest:tt)*) => {
        $crate::__fields!(__span (module_path!(), $crate::Parent::Current, $level, $name) [] [] $($rest)*)
    };
}

/// An event of `level`: `event!(Level::WARN, %dir, "text {x}")`, with `target:` and `parent:`
/// before the level as in the level macros.
#[macro_export]
macro_rules! event {
    (target: $target:expr, parent: $parent:expr, $level:expr, $($rest:tt)+) => {
        $crate::__fields!(__event ($target, $crate::Parent::of($parent), $level) [] [] $($rest)+)
    };
    (target: $target:expr, $level:expr, $($rest:tt)+) => {
        $crate::__fields!(__event ($target, $crate::Parent::Current, $level) [] [] $($rest)+)
    };
    (parent: $parent:expr, $level:expr, $($rest:tt)+) => {
        $crate::__fields!(__event (module_path!(), $crate::Parent::of($parent), $level) [] [] $($rest)+)
    };
    ($level:expr, $($rest:tt)+) => {
        $crate::__fields!(__event (module_path!(), $crate::Parent::Current, $level) [] [] $($rest)+)
    };
}

/// A span at `ERROR`: `error_span!([target: ..,] [parent: ..,] "name" [, field]*)`.
#[macro_export]
macro_rules! error_span {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, parent: $parent, $crate::Level::ERROR, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, $crate::Level::ERROR, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(parent: $parent, $crate::Level::ERROR, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::span!($crate::Level::ERROR, $($rest)+)
    };
}

/// An event at `ERROR`: `error!([target: ..,] [parent: ..,] [field,]* ["fmt" [, arg]*])`.
#[macro_export]
macro_rules! error {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, parent: $parent, $crate::Level::ERROR, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, $crate::Level::ERROR, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(parent: $parent, $crate::Level::ERROR, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::event!($crate::Level::ERROR, $($rest)+)
    };
}

/// A span at `WARN`: `warn_span!([target: ..,] [parent: ..,] "name" [, field]*)`.
#[macro_export]
macro_rules! warn_span {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, parent: $parent, $crate::Level::WARN, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, $crate::Level::WARN, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(parent: $parent, $crate::Level::WARN, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::span!($crate::Level::WARN, $($rest)+)
    };
}

/// An event at `WARN`: `warn!([target: ..,] [parent: ..,] [field,]* ["fmt" [, arg]*])`.
#[macro_export]
macro_rules! warn {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, parent: $parent, $crate::Level::WARN, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, $crate::Level::WARN, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(parent: $parent, $crate::Level::WARN, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::event!($crate::Level::WARN, $($rest)+)
    };
}

/// A span at `INFO`: `info_span!([target: ..,] [parent: ..,] "name" [, field]*)`.
#[macro_export]
macro_rules! info_span {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, parent: $parent, $crate::Level::INFO, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, $crate::Level::INFO, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(parent: $parent, $crate::Level::INFO, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::span!($crate::Level::INFO, $($rest)+)
    };
}

/// An event at `INFO`: `info!([target: ..,] [parent: ..,] [field,]* ["fmt" [, arg]*])`.
#[macro_export]
macro_rules! info {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, parent: $parent, $crate::Level::INFO, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, $crate::Level::INFO, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(parent: $parent, $crate::Level::INFO, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::event!($crate::Level::INFO, $($rest)+)
    };
}

/// A span at `DEBUG`: `debug_span!([target: ..,] [parent: ..,] "name" [, field]*)`.
#[macro_export]
macro_rules! debug_span {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, parent: $parent, $crate::Level::DEBUG, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, $crate::Level::DEBUG, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(parent: $parent, $crate::Level::DEBUG, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::span!($crate::Level::DEBUG, $($rest)+)
    };
}

/// An event at `DEBUG`: `debug!([target: ..,] [parent: ..,] [field,]* ["fmt" [, arg]*])`.
#[macro_export]
macro_rules! debug {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, parent: $parent, $crate::Level::DEBUG, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, $crate::Level::DEBUG, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(parent: $parent, $crate::Level::DEBUG, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::event!($crate::Level::DEBUG, $($rest)+)
    };
}

/// A span at `TRACE`, compiled out without debug assertions: `trace_span!([target: ..,]
/// [parent: ..,] "name" [, field]*)`.
#[macro_export]
macro_rules! trace_span {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, parent: $parent, $crate::Level::TRACE, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::span!(target: $target, $crate::Level::TRACE, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::span!(parent: $parent, $crate::Level::TRACE, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::span!($crate::Level::TRACE, $($rest)+)
    };
}

/// An event at `TRACE`, compiled out without debug assertions: `trace!([target: ..,]
/// [parent: ..,] [field,]* ["fmt" [, arg]*])`.
#[macro_export]
macro_rules! trace {
    (target: $target:expr, parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, parent: $parent, $crate::Level::TRACE, $($rest)+)
    };
    (target: $target:expr, $($rest:tt)+) => {
        $crate::event!(target: $target, $crate::Level::TRACE, $($rest)+)
    };
    (parent: $parent:expr, $($rest:tt)+) => {
        $crate::event!(parent: $parent, $crate::Level::TRACE, $($rest)+)
    };
    ($($rest:tt)+) => {
        $crate::event!($crate::Level::TRACE, $($rest)+)
    };
}
