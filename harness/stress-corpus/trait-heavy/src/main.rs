//! Trait-solver load in the shape of typenum, frunk and Diesel, written from scratch: type-level
//! Peano arithmetic recursing ~150 deep, an HList whose index is found by inference, and a query
//! builder with blanket impls, associated-type projections in impl headers, tuple impls up to
//! arity 16 and a filter chain whose type nests two dozen levels deep.
#![recursion_limit = "512"]
// Marker types (Z, Integer, Here, ...) exist only at the type level and are never built.
#![allow(dead_code)]

mod peano {
    use std::marker::PhantomData;

    pub struct Z;
    pub struct S<N>(PhantomData<N>);

    pub trait Nat {
        const VALUE: usize;
    }
    impl Nat for Z {
        const VALUE: usize = 0;
    }
    impl<N: Nat> Nat for S<N> {
        const VALUE: usize = N::VALUE + 1;
    }

    pub trait AddT<R> {
        type Out;
    }
    impl<R> AddT<R> for Z {
        type Out = R;
    }
    impl<N: AddT<R>, R> AddT<R> for S<N> {
        type Out = S<<N as AddT<R>>::Out>;
    }

    pub trait MulT<R> {
        type Out;
    }
    impl<R> MulT<R> for Z {
        type Out = Z;
    }
    impl<N, R> MulT<R> for S<N>
    where
        N: MulT<R>,
        <N as MulT<R>>::Out: AddT<R>,
    {
        type Out = <<N as MulT<R>>::Out as AddT<R>>::Out;
    }

    pub type Sum<A, B> = <A as AddT<B>>::Out;
    pub type Prod<A, B> = <A as MulT<B>>::Out;

    pub type N1 = S<Z>;
    pub type N2 = S<N1>;
    pub type N3 = S<N2>;
    pub type N4 = Sum<N2, N2>;
    pub type N8 = Sum<N4, N4>;
    pub type N10 = Sum<N8, N2>;
    pub type N12 = Sum<N8, N4>;
    pub type N100 = Prod<N10, N10>;
    pub type N144 = Prod<N12, N12>;

    pub trait SameAs<T> {}
    impl<T> SameAs<T> for T {}

    /// Compiles only when `A` and `B` normalize to the same type.
    pub fn assert_same<A: SameAs<B>, B>() {}
}

mod hlist {
    use std::marker::PhantomData;

    pub struct HNil;
    pub struct HCons<H, T> {
        pub head: H,
        pub tail: T,
    }

    pub struct Here;
    pub struct There<I>(PhantomData<I>);

    /// Takes the element of type `T` out of the list; `I` says where it is and is always inferred.
    pub trait Pluck<T, I> {
        type Rest;
        fn pluck(self) -> (T, Self::Rest);
    }
    impl<T, Tail> Pluck<T, Here> for HCons<T, Tail> {
        type Rest = Tail;
        fn pluck(self) -> (T, Tail) {
            (self.head, self.tail)
        }
    }
    impl<H, Tail, T, I> Pluck<T, There<I>> for HCons<H, Tail>
    where
        Tail: Pluck<T, I>,
    {
        type Rest = HCons<H, <Tail as Pluck<T, I>>::Rest>;
        fn pluck(self) -> (T, Self::Rest) {
            let (t, rest) = self.tail.pluck();
            (t, HCons { head: self.head, tail: rest })
        }
    }

    pub trait Field {
        fn value(&self) -> u32;
    }

    pub trait SumFields {
        fn sum_fields(&self) -> u32;
        const LEN: usize;
    }
    impl SumFields for HNil {
        fn sum_fields(&self) -> u32 {
            0
        }
        const LEN: usize = 0;
    }
    impl<H: Field, T: SumFields> SumFields for HCons<H, T> {
        fn sum_fields(&self) -> u32 {
            self.head.value() + self.tail.sum_fields()
        }
        const LEN: usize = T::LEN + 1;
    }

    macro_rules! fields {
        ($($name:ident),* $(,)?) => {
            $(
                #[derive(Debug, Clone, Copy, PartialEq)]
                pub struct $name(pub u32);
                impl Field for $name {
                    fn value(&self) -> u32 {
                        self.0
                    }
                }
            )*
        };
    }
    fields!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15);

    macro_rules! hlist {
        () => { HNil };
        ($head:expr $(, $tail:expr)* $(,)?) => {
            HCons { head: $head, tail: hlist!($($tail),*) }
        };
    }
    pub(crate) use hlist;
}

mod query {
    use std::marker::PhantomData;

    pub struct Integer;
    pub struct Text;
    pub struct Bool;
    pub struct Nullable<ST>(PhantomData<ST>);

    pub trait Expression {
        type SqlType;
    }

    pub trait QueryFragment {
        fn walk(&self, out: &mut String);
    }

    pub trait Table {
        const NAME: &'static str;
    }

    pub trait Column: Expression {
        type Table: Table;
        const NAME: &'static str;
    }

    /// `Self` may appear in a query over `QS`.
    pub trait AppearsOn<QS> {}
    impl<C: Column> AppearsOn<<C as Column>::Table> for C {}

    // Blanket impls through references and boxes.
    impl<T: Expression + ?Sized> Expression for Box<T> {
        type SqlType = T::SqlType;
    }
    impl<T: Expression + ?Sized> Expression for &T {
        type SqlType = T::SqlType;
    }
    impl<T: QueryFragment + ?Sized> QueryFragment for Box<T> {
        fn walk(&self, out: &mut String) {
            (**self).walk(out)
        }
    }
    impl<T: QueryFragment + ?Sized> QueryFragment for &T {
        fn walk(&self, out: &mut String) {
            (**self).walk(out)
        }
    }
    // No `AppearsOn` through `Box<T>` or `&T`: both are fundamental, so a downstream crate could
    // implement `Column` for them and the column blanket impl above would overlap.

    pub struct Bound<T, ST>(pub T, pub PhantomData<ST>);
    impl<T, ST> Expression for Bound<T, ST> {
        type SqlType = ST;
    }
    impl<T, ST, QS> AppearsOn<QS> for Bound<T, ST> {}

    pub trait SqlLiteral {
        fn literal(&self, out: &mut String);
    }
    impl SqlLiteral for i32 {
        fn literal(&self, out: &mut String) {
            out.push_str(&self.to_string());
        }
    }
    impl SqlLiteral for &'static str {
        fn literal(&self, out: &mut String) {
            out.push('\'');
            out.push_str(self);
            out.push('\'');
        }
    }
    impl<T: SqlLiteral, ST> QueryFragment for Bound<T, ST> {
        fn walk(&self, out: &mut String) {
            self.0.literal(out)
        }
    }

    /// A Rust value usable where an expression of SQL type `ST` is expected.
    pub trait AsExpr<ST> {
        type Expr: Expression<SqlType = ST>;
        fn as_expr(self) -> Self::Expr;
    }
    impl AsExpr<Integer> for i32 {
        type Expr = Bound<i32, Integer>;
        fn as_expr(self) -> Self::Expr {
            Bound(self, PhantomData)
        }
    }
    impl AsExpr<Text> for &'static str {
        type Expr = Bound<&'static str, Text>;
        fn as_expr(self) -> Self::Expr {
            Bound(self, PhantomData)
        }
    }
    impl<E: Expression> AsExpr<E::SqlType> for E {
        type Expr = E;
        fn as_expr(self) -> E {
            self
        }
    }

    macro_rules! binary {
        ($name:ident, $op:literal, $out:ty, $($bound:tt)*) => {
            pub struct $name<L, R>(pub L, pub R);
            impl<L, R> Expression for $name<L, R>
            where
                L: Expression,
                R: Expression<SqlType = $($bound)*>,
            {
                type SqlType = $out;
            }
            impl<L: AppearsOn<QS>, R: AppearsOn<QS>, QS> AppearsOn<QS> for $name<L, R> {}
            impl<L: QueryFragment, R: QueryFragment> QueryFragment for $name<L, R> {
                fn walk(&self, out: &mut String) {
                    out.push('(');
                    self.0.walk(out);
                    out.push_str($op);
                    self.1.walk(out);
                    out.push(')');
                }
            }
        };
    }
    binary!(Equals, " = ", Bool, L::SqlType);
    binary!(Greater, " > ", Bool, L::SqlType);
    binary!(AndExpr, " AND ", Bool, Bool);
    binary!(OrExpr, " OR ", Bool, Bool);

    pub struct IsNull<E>(pub E);
    impl<ST, E: Expression<SqlType = Nullable<ST>>> Expression for IsNull<E> {
        type SqlType = Bool;
    }
    impl<E: AppearsOn<QS>, QS> AppearsOn<QS> for IsNull<E> {}
    impl<E: QueryFragment> QueryFragment for IsNull<E> {
        fn walk(&self, out: &mut String) {
            self.0.walk(out);
            out.push_str(" IS NULL");
        }
    }

    /// Methods on every expression, through one blanket impl.
    pub trait ExprMethods: Expression + Sized {
        fn equals<R: AsExpr<Self::SqlType>>(self, r: R) -> Equals<Self, R::Expr> {
            Equals(self, r.as_expr())
        }
        fn greater<R: AsExpr<Self::SqlType>>(self, r: R) -> Greater<Self, R::Expr> {
            Greater(self, r.as_expr())
        }
        fn and<R: Expression<SqlType = Bool>>(self, r: R) -> AndExpr<Self, R>
        where
            Self: Expression<SqlType = Bool>,
        {
            AndExpr(self, r)
        }
        fn or<R: Expression<SqlType = Bool>>(self, r: R) -> OrExpr<Self, R>
        where
            Self: Expression<SqlType = Bool>,
        {
            OrExpr(self, r)
        }
        fn is_null<ST>(self) -> IsNull<Self>
        where
            Self: Expression<SqlType = Nullable<ST>>,
        {
            IsNull(self)
        }
    }
    impl<T: Expression> ExprMethods for T {}

    macro_rules! tuple_impls {
        ($($T:ident),+) => {
            impl<$($T: Expression),+> Expression for ($($T,)+) {
                type SqlType = ($($T::SqlType,)+);
            }
            impl<QS, $($T: AppearsOn<QS>),+> AppearsOn<QS> for ($($T,)+) {}
            impl<$($T: QueryFragment),+> QueryFragment for ($($T,)+) {
                #[allow(non_snake_case)]
                fn walk(&self, out: &mut String) {
                    let ($($T,)+) = self;
                    let mut first = true;
                    $(
                        if !first {
                            out.push_str(", ");
                        }
                        first = false;
                        $T.walk(out);
                    )+
                    let _ = first;
                }
            }
        };
    }
    macro_rules! tuple_impls_all {
        () => {};
        ($head:ident $($rest:ident)*) => {
            tuple_impls!($head $(, $rest)*);
            tuple_impls_all!($($rest)*);
        };
    }
    tuple_impls_all!(A B C D E F G H I J K L M N O P);

    pub struct NoFilter;
    pub struct Where<P>(P);

    pub trait AddFilter<P> {
        type Out;
        fn add(self, p: P) -> Self::Out;
    }
    impl<P> AddFilter<P> for NoFilter {
        type Out = Where<P>;
        fn add(self, p: P) -> Where<P> {
            Where(p)
        }
    }
    impl<W, P> AddFilter<P> for Where<W>
    where
        W: Expression<SqlType = Bool>,
        P: Expression<SqlType = Bool>,
    {
        type Out = Where<AndExpr<W, P>>;
        fn add(self, p: P) -> Self::Out {
            Where(AndExpr(self.0, p))
        }
    }

    pub trait FilterFragment {
        fn walk_filter(&self, out: &mut String);
    }
    impl FilterFragment for NoFilter {
        fn walk_filter(&self, _out: &mut String) {}
    }
    impl<P: QueryFragment> FilterFragment for Where<P> {
        fn walk_filter(&self, out: &mut String) {
            out.push_str(" WHERE ");
            self.0.walk(out);
        }
    }

    pub struct Select<S, F, W> {
        sel: S,
        _from: PhantomData<F>,
        filter: W,
    }

    pub fn select_from<F: Table, S: Expression + AppearsOn<F>>(_from: F, sel: S) -> Select<S, F, NoFilter> {
        Select { sel, _from: PhantomData, filter: NoFilter }
    }

    impl<S, F, W> Select<S, F, W> {
        pub fn filter<P>(self, p: P) -> Select<S, F, <W as AddFilter<P>>::Out>
        where
            P: Expression<SqlType = Bool> + AppearsOn<F>,
            W: AddFilter<P>,
        {
            Select { sel: self.sel, _from: PhantomData, filter: self.filter.add(p) }
        }
    }

    impl<S: QueryFragment, F: Table, W: FilterFragment> Select<S, F, W> {
        pub fn to_sql(&self) -> String {
            let mut out = String::from("SELECT ");
            self.sel.walk(&mut out);
            out.push_str(" FROM ");
            out.push_str(F::NAME);
            self.filter.walk_filter(&mut out);
            out
        }
    }

    macro_rules! table {
        ($table:ident { $($col:ident : $ty:ty),+ $(,)? }) => {
            #[allow(non_camel_case_types)]
            pub mod $table {
                use super::*;

                #[derive(Clone, Copy)]
                pub struct table;
                impl Table for table {
                    const NAME: &'static str = stringify!($table);
                }
                $(
                    #[derive(Clone, Copy)]
                    pub struct $col;
                    impl Expression for $col {
                        type SqlType = $ty;
                    }
                    impl Column for $col {
                        type Table = table;
                        const NAME: &'static str = stringify!($col);
                    }
                    impl QueryFragment for $col {
                        fn walk(&self, out: &mut String) {
                            out.push_str(stringify!($table));
                            out.push('.');
                            out.push_str(<$col as Column>::NAME);
                        }
                    }
                )+
            }
        };
    }
    table!(users { id: Integer, name: Text, age: Integer, email: Nullable<Text> });
    table!(posts { id: Integer, user_id: Integer, title: Text, body: Text, score: Integer });
}

fn peano_checks() {
    use peano::*;
    assert_eq!(<N144 as Nat>::VALUE, 144);
    assert_eq!(<N100 as Nat>::VALUE, 100);
    assert_same::<N144, Prod<Prod<N3, N4>, N12>>();
    assert_same::<Sum<N100, N12>, Sum<N12, N100>>();
}

fn hlist_checks() {
    use hlist::*;
    let list = hlist![
        T0(0), T1(1), T2(4), T3(9), T4(16), T5(25), T6(36), T7(49),
        T8(64), T9(81), T10(100), T11(121), T12(144), T13(169), T14(196), T15(225),
    ];
    assert_eq!(list.sum_fields(), 1240);
    let (eleven, rest): (T11, _) = list.pluck();
    assert_eq!(eleven, T11(121));
    assert_eq!(rest.sum_fields(), 1119);
    let (fifteen, rest): (T15, _) = rest.pluck();
    let (zero, rest): (T0, _) = rest.pluck();
    assert_eq!((fifteen.0, zero.0), (225, 0));
    assert_eq!(rest.sum_fields(), 1119 - 225);
    fn len_of<L: SumFields>(_: &L) -> usize {
        L::LEN
    }
    assert_eq!(len_of(&rest), 13);
}

fn query_checks() {
    use query::*;

    let q = select_from(users::table, (users::id, users::name))
        .filter(users::id.equals(1))
        .filter(users::name.equals("ann"));
    assert_eq!(
        q.to_sql(),
        "SELECT users.id, users.name FROM users WHERE ((users.id = 1) AND (users.name = 'ann'))"
    );

    let q = select_from(users::table, users::email).filter(users::email.is_null().or(users::age.greater(users::id)));
    assert_eq!(q.to_sql(), "SELECT users.email FROM users WHERE (users.email IS NULL OR (users.age > users.id))");

    // The columns of a table, repeated into one sixteen-wide tuple.
    let wide = (
        posts::id, posts::user_id, posts::title, posts::body, posts::score, posts::id, posts::user_id, posts::title,
        posts::body, posts::score, posts::id, posts::user_id, posts::title, posts::body, posts::score, posts::id,
    );
    let q = select_from(posts::table, wide).filter(posts::score.greater(10));
    assert!(q.to_sql().starts_with("SELECT posts.id, posts.user_id, posts.title, posts.body, posts.score, posts.id"));
    assert!(q.to_sql().ends_with("FROM posts WHERE (posts.score > 10)"));

    // A filter chain whose type nests 23 levels of AndExpr, each level a projection to normalize.
    let q = select_from(posts::table, (posts::id,))
        .filter(posts::score.greater(0))
        .filter(posts::score.greater(1))
        .filter(posts::score.greater(2))
        .filter(posts::score.greater(3))
        .filter(posts::user_id.equals(4))
        .filter(posts::user_id.equals(5).or(posts::user_id.equals(6)))
        .filter(posts::title.equals("a"))
        .filter(posts::body.equals("b"))
        .filter(posts::score.greater(8))
        .filter(posts::score.greater(9))
        .filter(posts::score.greater(10))
        .filter(posts::score.greater(11))
        .filter(posts::id.greater(posts::user_id))
        .filter(posts::id.equals(13).and(posts::score.equals(13)))
        .filter(posts::score.greater(14))
        .filter(posts::score.greater(15))
        .filter(posts::score.greater(16))
        .filter(posts::score.greater(17))
        .filter(posts::score.greater(18))
        .filter(posts::score.greater(19))
        .filter(posts::score.greater(20))
        .filter(posts::score.greater(21))
        .filter(posts::score.greater(22))
        .filter(posts::score.greater(23));
    let sql = q.to_sql();
    assert!(sql.starts_with("SELECT posts.id FROM posts WHERE ((((((((((((((((((((((((posts.score > 0)"));
    assert!(sql.ends_with(" AND (posts.score > 23))"));
    assert_eq!(sql.matches(" AND ").count(), 24);

    // Fragments through the `Box<T>` and `&T` blanket impls and a trait object.
    fn render<Q: QueryFragment>(q: Q) -> String {
        let mut out = String::new();
        q.walk(&mut out);
        out
    }
    let parts: [Box<dyn QueryFragment>; 2] = [Box::new(users::id), Box::new(users::name.equals("x"))];
    assert_eq!(render(&parts[0]), "users.id");
    assert_eq!(render(&parts[1]), "(users.name = 'x')");
}

fn main() {
    peano_checks();
    hlist_checks();
    query_checks();
    println!("trait-heavy ok");
}
