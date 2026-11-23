use blaze_ids::ContentId;
use blaze_proc_macro::script_sandbox::Read;
use blaze_proc_macro::spawn_route::{PinnedTool, SpawnSetting};

use crate::engine_script_reads::{NoCell, ReadList, ReadsPlant};
use crate::scripts::{RanAs, admit_reads};

fn observed(subject: &str, digest: ContentId) -> Read {
    Read { subject: subject.to_owned(), digest: Some(digest) }
}

fn admit(observed: &[Read], setting: &SpawnSetting) -> Result<ReadList, NoCell> {
    let heap = blaze_own::testing::heap();
    let mut cells = ReadList::new_in(heap.shared());
    admit_reads(observed, RanAs::Wasm, setting, None, ReadsPlant::None, None, &mut cells, heap.alloc())?;
    Ok(cells)
}

#[test]
fn executor_receipts_require_the_same_request_and_current_tool_grant() {
    let alloc = blaze_own::testing::heap().alloc();
    let tool = |name: &str, bytes: &[u8]| PinnedTool {
        name: name.to_owned(),
        guest_path: format!("/tools/{name}"),
        digest: blaze_sha256::digest(bytes),
    };
    let mut setting = SpawnSetting {
        hvbox_allowed: true,
        userland: Some(ContentId([3; 32])),
        tools: vec![tool("probe", b"first image"), tool("sh", b"shell")],
        pins: None,
    };
    let grant = setting.executor_id(alloc).expect("a complete executor grant");
    let key = "0123456789abcdef".repeat(4);
    let answer = format!("spawn:{key}");
    let proof = format!("spawn-grant:{key}");
    let reply = ContentId::of("test executor returned bytes", b"exit 0\n");
    let complete = [observed(&proof, grant), observed(&answer, reply)];
    let cells = admit(&complete, &setting).expect("both receipts of this request, in either order");
    assert_eq!(cells.len(), 1);
    assert_eq!((cells[0].path.as_str(), cells[0].answer), (crate::sweep::EXECUTOR_FILE, grant));

    for incomplete in [
        vec![observed(&answer, reply)],
        vec![observed(&proof, grant)],
        vec![observed("spawn:123", reply), observed(&proof, grant)],
        vec![observed(&answer, reply), observed(&format!("spawn-grant:{}", "a".repeat(64)), grant)],
        vec![observed(&answer, reply), observed(&proof, grant), observed(&answer, ContentId([9; 32]))],
    ] {
        assert!(admit(&incomplete, &setting).is_err(), "an incomplete or conflicting receipt cannot keep the script");
    }

    setting.tools.reverse();
    assert_eq!(setting.executor_id(alloc), Some(grant), "grant order is unobservable");
    assert!(admit(&complete, &setting).is_ok());
    let original = setting.tools[0].clone();
    setting.tools[0].digest = blaze_sha256::digest(b"replacement shell");
    assert!(admit(&complete, &setting).is_err(), "the pinned executable changed");
    setting.tools[0] = original;
    setting.tools[0].guest_path = "/relocated/sh".to_owned();
    assert!(admit(&complete, &setting).is_err(), "the guest path selecting the executable changed");
    setting.tools[0].guest_path = setting.tools[1].guest_path.clone();
    assert_eq!(setting.executor_id(alloc), None, "one guest executable cannot resolve to two program images");
    assert!(admit(&complete, &setting).is_err());
    println!("executor receipt: matching pair kept; incomplete, conflicting and changed grants rejected");
}
