use blaze_drive::{Kind, Locate, Name, Part};
use blaze_engine::SVec;
use blaze_ids::ContentId;

#[test]
fn skipped_target_has_no_body_task() {
    let alloc = blaze_own::testing::heap().shared();
    let file = Name::file(b"src/bin/x.rs");
    let body = Name::new(Kind::BodyCheck, ContentId::of("skipped target body", b"x"));
    let mut record = crate::work::Record::new_in(file, alloc);
    record.parts.push(Part { name: body, locate: Locate::Source(ContentId::of("skipped target source", b"x")), place: 0 });
    let mut path = SVec::new_in(alloc);
    path.extend_from_slice(b"src/bin/x.rs");
    let outlined = [crate::engine_tasks::Outlined {
        name: file,
        path,
        source: ContentId::of("skipped target file", b"x"),
        outline_source: ContentId::of("skipped target outline", b"x"),
        edition: 3,
        made: Ok(record),
    }];
    assert!(crate::engine_bodies::plan_views::<blaze_drive::view::Production>(&outlined, &[file], &[], None, alloc).is_empty());
    assert_eq!(crate::engine_bodies::plan_views::<blaze_drive::view::Production>(&outlined, &[], &[], None, alloc).len(), 1);
}
