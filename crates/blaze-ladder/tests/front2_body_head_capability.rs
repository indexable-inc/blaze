use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::front2::tables::view::{BodyHeadFacts, Tables};

#[test]
fn body_head_facts_cannot_be_used_as_a_text_source() {
    type Facts = BodyHeadFacts<'static, 'static, Tables<'static, 'static>>;

    // For a source that can read arbitrary items both implementations apply, so
    // Rust rejects the ambiguous marker. The body head view must have only one.
    trait Probe<Marker> {
        fn probe() {}
    }
    struct TextSource;
    impl<T> Probe<()> for T {}
    impl<T: NamesSource<'static>> Probe<TextSource> for T {}

    <Facts as Probe<_>>::probe();
}
