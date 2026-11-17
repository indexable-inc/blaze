use super::*;

/// The invariant: a test run outside the sandbox is keyed apart from the same test run inside,
/// so no pass kept of a sandboxed run is ever read for it; the control is the suite's own
/// `given`, which is stable.
#[test]
fn a_run_outside_the_sandbox_is_keyed_apart_from_one_inside() {
    let suite = Suite {
        package: "sel".to_owned(),
        target: "lib".to_owned(),
        context: ContentId::of("blaze test select context", b"x86_64-unknown-linux-gnu"),
    };
    assert_eq!(suite.given(), suite.given());
    assert_eq!(suite.given_outside(), suite.given_outside());
    assert_ne!(suite.given(), suite.given_outside());
}

fn ignored() -> Attrs {
    Attrs { ignore: true, ..Attrs::default() }
}

#[test]
fn filters_and_ignored_tests_as_libtest_has_them() {
    let (quick, slow) = (("parse::quick", Attrs::default()), ("parse::slow", ignored()));
    let ask = |test: &(&str, Attrs), filter: &Filter| asked(test.0, &test.1, filter);
    let all = Filter::default();
    assert_eq!(ask(&quick, &all), Wanted::Yes);
    assert_eq!(ask(&slow, &all), Wanted::Ignored);
    let only = Filter { ignored: Ignored::Only, ..Filter::default() };
    assert_eq!((ask(&quick, &only), ask(&slow, &only)), (Wanted::Filtered, Wanted::Yes));
    let include = Filter { ignored: Ignored::Include, ..Filter::default() };
    assert_eq!(ask(&slow, &include), Wanted::Yes);
    let named = Filter { names: vec!["quick".into()], ..Filter::default() };
    assert_eq!((ask(&quick, &named), ask(&slow, &named)), (Wanted::Yes, Wanted::Filtered));
    let exact = Filter { names: vec!["quick".into()], exact: true, ..Filter::default() };
    assert_eq!(ask(&quick, &exact), Wanted::Filtered);
    let skip = Filter { skip: vec!["parse::".into()], ..Filter::default() };
    assert_eq!(ask(&quick, &skip), Wanted::Filtered);
}

/// Whether a test is asked for is a scan of the filter's names: nothing allocated.
#[test]
fn a_filter_allocates_nothing() {
    let (quick, slow) = (Attrs::default(), ignored());
    let filters = [
        Filter::default(),
        Filter { names: vec!["quick".into(), "lex".into()], ..Filter::default() },
        Filter { names: vec!["parse::quick".into()], exact: true, ..Filter::default() },
        Filter { skip: vec!["slow".into()], ignored: Ignored::Include, ..Filter::default() },
    ];
    let wanted = blaze_alloc_count::assert_at_most(0, "a test's filter checked", || {
        filters
            .each_ref()
            .map(|filter| (asked("parse::quick", &quick, filter), asked("parse::slow", &slow, filter)))
    });
    assert_eq!(
        wanted,
        [
            (Wanted::Yes, Wanted::Ignored),
            (Wanted::Yes, Wanted::Filtered),
            (Wanted::Yes, Wanted::Filtered),
            (Wanted::Yes, Wanted::Filtered),
        ]
    );
}
