//! The crate's own tests: frames and the handshake's bodies fuzzed (never a panic on any
//! bytes), the grant's checks against an image's layout, the code memory's rules, and on
//! Linux the real thing: a running thread's code laid over by the memory object with every
//! byte equal at the swap, a patch one word store it sees, the executable written once per
//! key, and the host's handshake over a real socket.

use blaze_ids::rng::SplitMix64;

use crate::frame::{self, FrameError, Header, Kind, Reader, Writer, kinds};
use crate::handshake::{
    Grant, Hello, Lookup, MOST_BINDS, MOST_NAME, OVERLAY_BYTES, Overlay, Place, VERSION, hex, host_var, unhex,
};
use crate::image::tests::{R, RX, elf};
use crate::sys::Rights;
use crate::{Access, CodeMemory, CodeRange, Format, ImageWrite, Layout, Segment};

fn random_bytes(rng: &mut SplitMix64, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    rng.fill(&mut bytes);
    bytes
}

#[test]
fn headers_round_trip_and_refuse_reserved_bits_and_long_bodies() {
    let header = Header { kind: Kind(0x0123), len: 77, seq: 0xdead_beef };
    assert_eq!(Header::decode(&header.encode()), Ok(header));
    let mut bytes = header.encode();
    bytes[2] = 1;
    assert_eq!(Header::decode(&bytes), Err(FrameError::Reserved { bits: 1 }));
    let long = Header { kind: Kind(1), len: frame::MOST_BODY + 1, seq: 0 }.encode();
    assert!(matches!(Header::decode(&long), Err(FrameError::TooLong { .. })));
}

#[test]
fn frames_round_trip_whole_and_wait_when_cut() {
    let mut rng = SplitMix64::new(0x1234_5678_9abc_def1);
    for _ in 0..500 {
        let len = rng.index(300);
        let body = random_bytes(&mut rng, len);
        let kind = Kind(rng.next_u64() as u16);
        let seq = rng.next_u32();
        let mut bytes = Vec::new();
        frame::encode(kind, seq, &body, &mut bytes).unwrap();
        let (decoded, used) = frame::decode(&bytes).unwrap().unwrap();
        assert_eq!((decoded.kind, decoded.seq, decoded.body, used), (kind, seq, body.as_slice(), bytes.len()));
        let cut = rng.index(bytes.len());
        assert_eq!(frame::decode(&bytes[..cut]), Ok(None));
        let mut two = bytes.clone();
        frame::encode(Kind(9), 1, b"next", &mut two).unwrap();
        let (_, first) = frame::decode(&two).unwrap().unwrap();
        let (second, _) = frame::decode(&two[first..]).unwrap().unwrap();
        assert_eq!(second.body, b"next");
    }
}

#[test]
fn no_bytes_make_a_frame_panic() {
    let mut rng = SplitMix64::new(0x0bad_cafe_f00d_0001);
    for round in 0..20_000 {
        let len = rng.index(64);
        let mut bytes = random_bytes(&mut rng, len);
        if round % 2 == 0 && bytes.len() >= frame::HEADER {
            // Mostly well-formed headers, so the body paths are reached too.
            bytes[2] = 0;
            bytes[3] = 0;
            let body = rng.below(80) as u32;
            bytes[4..8].copy_from_slice(&body.to_le_bytes());
        }
        if let Ok(Some((frame, used))) = frame::decode(&bytes) {
            assert!(used <= bytes.len());
            assert_eq!(used, frame::HEADER + frame.body.len());
        }
        let mut read = Reader::new(&bytes);
        for _ in 0..8 {
            let before = read.left();
            let ok = match rng.below(9) {
                0 => read.u8().is_ok(),
                1 => read.u16().is_ok(),
                2 => read.u32().is_ok(),
                3 => read.u64().is_ok(),
                4 => read.i64().is_ok(),
                5 => read.bytes().is_ok(),
                6 => read.str().is_ok(),
                7 => {
                    let each = rng.index(40);
                    match read.count(each) {
                        Ok(count) => {
                            assert!(count.saturating_mul(each.max(1)) <= read.left());
                            true
                        }
                        Err(_) => false,
                    }
                }
                _ => read.raw(rng.index(16)).is_ok(),
            };
            assert!(read.left() <= before);
            if !ok {
                break;
            }
        }
        let _ = read.finish();
    }
}

#[test]
fn fields_round_trip() {
    let mut body = Vec::new();
    Writer::new(&mut body).u8(1).u16(2).u32(3).u64(4).i64(-5).bytes(b"six").str("seven").raw(&[8, 8]);
    let mut read = Reader::new(&body);
    assert_eq!(read.u8(), Ok(1));
    assert_eq!(read.u16(), Ok(2));
    assert_eq!(read.u32(), Ok(3));
    assert_eq!(read.u64(), Ok(4));
    assert_eq!(read.i64(), Ok(-5));
    assert_eq!(read.bytes(), Ok(&b"six"[..]));
    assert_eq!(read.str(), Ok("seven"));
    assert_eq!(read.array::<2>(), Ok([8, 8]));
    assert_eq!(read.finish(), Ok(()));
    let mut bad = Vec::new();
    Writer::new(&mut bad).bytes(&[0xff, 0xfe]);
    assert_eq!(Reader::new(&bad).str(), Err(FrameError::Utf8));
    assert!(matches!(Reader::new(&[1, 2]).finish(), Err(FrameError::Trailing { left: 2 })));
    let frame = frame::Frame { kind: kinds::HELLO, seq: 0, body: &[] };
    assert!(frame.reader(kinds::HELLO).is_ok());
    assert_eq!(frame.reader(kinds::GRANT).err(), Some(FrameError::Kind { kind: kinds::HELLO }));
}

#[test]
fn kind_ranges_are_apart() {
    assert!(kinds::INPROC.end <= kinds::LAZY.start);
    assert!(kinds::LAZY.end <= kinds::PREMAP.start);
    assert!(kinds::PREMAP.end <= kinds::LIVE.start);
    let all = [
        kinds::HELLO,
        kinds::GRANT,
        kinds::MAP,
        kinds::MAPPED,
        kinds::STARTED,
        kinds::FAILED,
        kinds::REFUSED,
        kinds::OVERLAY,
        kinds::OVERLAID,
    ];
    for (index, kind) in all.iter().enumerate() {
        assert!(kinds::INPROC.contains(&kind.0));
        assert!(all[..index].iter().all(|it| it != kind));
    }
}

#[test]
fn hellos_round_trip_and_refuse_cuts() {
    let hello = Hello { token: core::array::from_fn(|it| it as u8), pid: 77, version: VERSION, page: 0x4000, header: 0x5555_0000_0000 };
    let mut bytes = Vec::new();
    hello.encode(&mut bytes);
    assert_eq!(Hello::decode(&bytes), Ok(hello));
    for cut in 0..bytes.len() {
        assert!(Hello::decode(&bytes[..cut]).is_err());
    }
    bytes.push(0);
    assert!(Hello::decode(&bytes).is_err());
}

const P: u64 = 0x1000;

/// An image of headers, one code segment and two reservations, the second for fixed maps.
fn a_layout() -> Layout {
    Layout {
        format: Format::Elf,
        header: 0x10_0000,
        span: 0x10_0000..0x20_0000,
        code: vec![Segment { offset: P, linked: 0x10_0000 + P, len: 2 * P }],
        reserved: vec![0x10_8000..0x11_0000, 0x11_0000..0x20_0000],
    }
}

fn a_grant() -> Grant {
    let layout = a_layout();
    Grant {
        format: Format::Elf,
        header: layout.header,
        span: layout.span,
        objects: vec![
            (0x10_0000, Rights::Code),
            (0x2_0000, Rights::Words),
            (crate::ask::BYTES, Rights::Words),
            (P, Rights::Words),
            (0x10_0000, Rights::Code),
        ],
        overlays: vec![
            Overlay {
                object: 0,
                offset: P,
                len: 2 * P,
                at: 0x10_0000 + P,
                access: Access::Execute,
                place: Place::Code,
                image: false,
            },
            Overlay {
                object: 0,
                offset: 4 * P,
                len: 4 * P,
                at: 0x10_8000,
                access: Access::Execute,
                place: Place::Reserved,
                image: false,
            },
            Overlay { object: 3, offset: 0, len: P, at: 0x10_c000, access: Access::Shared, place: Place::Reserved, image: false },
            Overlay { object: 4, offset: 0, len: 8 * P, at: 0x11_0000, access: Access::Copy, place: Place::Reserved, image: true },
        ],
        writes: vec![
            ImageWrite { place: 0x11_0010, value: 0x11_2000, address: true },
            ImageWrite { place: 0x11_0000 + 8 * P - 8, value: 0x40, address: false },
        ],
        lookups: vec![
            Lookup { place: 0x11_0020, addend: 0, weak: false, name: 0, len: 6 },
            Lookup { place: 0x11_0028, addend: -16, weak: true, name: 6, len: 10 },
        ],
        names: "malloc__rg_alloc".to_owned(),
    }
}

fn encoded(grant: &Grant) -> Vec<u8> {
    let mut bytes = Vec::new();
    grant.encode(&mut bytes);
    bytes
}

#[test]
fn grants_round_trip() {
    let grant = a_grant();
    assert_eq!(Grant::decode(&encoded(&grant)), Ok(grant));
}

#[test]
fn a_grant_cut_or_mutated_is_an_error_not_a_panic() {
    let bytes = encoded(&a_grant());
    for cut in 0..bytes.len() {
        assert!(Grant::decode(&bytes[..cut]).is_err(), "a grant cut at {cut} was read");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(Grant::decode(&longer).is_err());
    let mut rng = SplitMix64::new(0x5eed_0000_0000_0007);
    for _ in 0..20_000 {
        let mut mutated = bytes.clone();
        for _ in 0..1 + rng.below(3) {
            let at = rng.index(mutated.len());
            mutated[at] = rng.next_u64() as u8;
        }
        if let Ok(grant) = Grant::decode(&mutated) {
            assert!(grant.objects.len() >= 3);
            let first: Vec<Rights> = grant.objects[..3].iter().map(|it| it.1).collect();
            assert_eq!(first, [Rights::Code, Rights::Words, Rights::Words]);
            for overlay in &grant.overlays {
                assert!(overlay.object != 1 && overlay.object != 2 && (overlay.object as usize) < grant.objects.len());
                let rights = grant.objects[overlay.object as usize].1;
                assert!(rights != Rights::Code || overlay.access != Access::Shared, "code shared writable");
                assert!(rights != Rights::Words || overlay.access == Access::Shared, "words executable");
                assert!(overlay.place != Place::Code || (overlay.object == 0 && overlay.access == Access::Execute));
                assert!(grant.span.start <= overlay.at && overlay.range().end <= grant.span.end);
            }
            assert!(grant.overlays.windows(2).all(|it| it[0].range().end <= it[1].at));
            grant.check_binds().unwrap();
        }
        let len = rng.index(128);
        let _ = Grant::decode(&random_bytes(&mut rng, len));
    }
}

#[test]
fn a_grant_of_rights_an_overlay_must_not_have_is_refused() {
    let refused = |change: &dyn Fn(&mut Grant)| {
        let mut grant = a_grant();
        change(&mut grant);
        Grant::decode(&encoded(&grant)).unwrap_err()
    };
    refused(&|it| it.overlays[2].object = 1);
    refused(&|it| it.overlays[2].object = 2);
    refused(&|it| it.objects.swap(0, 1));
    refused(&|it| it.objects[2].1 = Rights::Code);
    refused(&|it| it.overlays[0].access = Access::Shared);
    refused(&|it| it.overlays[3].access = Access::Shared);
    refused(&|it| it.overlays[2].access = Access::Execute);
    refused(&|it| it.overlays[3].place = Place::Code);
    refused(&|it| it.overlays[0].object = 4);
    refused(&|it| it.overlays[1].at = 0x10_0000 + 2 * P);
    refused(&|it| it.overlays.swap(1, 2));
    refused(&|it| it.overlays[3].len = 0x10_0000);
    refused(&|it| it.overlays[3].offset = 0x10_0000 - P);
    refused(&|it| it.objects.truncate(2));
}

#[test]
fn a_grant_is_checked_against_the_image_relative_to_its_header() {
    let grant = Grant::decode(&encoded(&a_grant())).unwrap();
    let mut own = a_layout();
    grant.check(&own, P).unwrap();
    // The system loader put this image elsewhere: the same grant, slid.
    let slide = 0x7f00_0000_0000;
    own.header += slide;
    own.span = own.span.start + slide..own.span.end + slide;
    own.code[0].linked += slide;
    own.reserved = own.reserved.iter().map(|it| it.start + slide..it.end + slide).collect();
    grant.check(&own, P).unwrap();

    let refused = |change: &dyn Fn(&mut Grant), why: &str| {
        let mut grant = a_grant();
        change(&mut grant);
        let error = grant.check(&own, P).unwrap_err();
        assert!(error.what.contains(why), "{error}");
    };
    refused(&|it| it.overlays[0].offset = 2 * P, "not a code segment");
    refused(&|it| it.overlays[0].len = P, "not a code segment");
    refused(&|it| it.overlays[0].place = Place::Reserved, "not inside a reservation");
    refused(&|it| it.overlays[1].at = 0x10_0000 + 4 * P, "not inside a reservation");
    refused(&|it| drop(it.overlays.remove(0)), "has no overlay");
    refused(&|it| it.span.end += P, "not the executable the daemon wrote");
    refused(&|it| it.format = Format::MachO, "a grant for a MachO image");
    refused(&|it| it.overlays[2].len = P / 2, "not whole pages");
}

/// Where the writes' count is in an encoded grant: after the header, the objects and the
/// overlays.
fn writes_at(grant: &Grant) -> usize {
    25 + 4 + 9 * grant.objects.len() + 4 + OVERLAY_BYTES * grant.overlays.len()
}

#[test]
fn a_grants_writes_and_lookups_round_trip_and_refuse_short_or_oversized_counts() {
    let grant = a_grant();
    let decoded = Grant::decode(&encoded(&grant)).unwrap();
    assert_eq!(decoded.writes, grant.writes);
    assert_eq!(decoded.lookups.iter().map(|it| decoded.name(it)).collect::<Vec<_>>(), ["malloc", "__rg_alloc"]);
    assert_eq!(decoded, grant);

    let empty = Grant { writes: Vec::new(), lookups: Vec::new(), names: String::new(), ..a_grant() };
    assert_eq!(Grant::decode(&encoded(&empty)), Ok(empty));

    let bytes = encoded(&grant);
    let writes = writes_at(&grant);
    assert_eq!(bytes[writes..writes + 4], 2u32.to_le_bytes());
    let lookups = writes + 4 + 17 * grant.writes.len();
    assert_eq!(bytes[lookups..lookups + 4], 2u32.to_le_bytes());
    // Counts past the bytes there are refused before anything is allocated for them.
    for (at, count) in [(writes, u32::MAX), (writes, 3), (lookups, u32::MAX), (lookups, 3), (lookups, 1 << 30)] {
        let mut forged = bytes.clone();
        forged[at..at + 4].copy_from_slice(&count.to_le_bytes());
        assert!(Grant::decode(&forged).is_err(), "a count of {count} at {at} was read");
    }
    // A name's length past the body, and past the most a name may have.
    let name = lookups + 4 + 17;
    assert_eq!(bytes[name..name + 4], 6u32.to_le_bytes());
    let mut forged = bytes;
    forged[name..name + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Grant::decode(&forged).is_err());
    let long = "x".repeat(MOST_NAME + 1);
    let oversized = Grant {
        lookups: vec![Lookup { place: 0x11_0020, addend: 0, weak: false, name: 0, len: long.len() as u32 }],
        names: long.clone(),
        ..a_grant()
    };
    assert!(oversized.check_binds().is_err());
    let error = Grant::decode(&encoded(&oversized)).unwrap_err();
    assert!(error.what.contains(&format!("{} bytes", MOST_NAME + 1)), "{error}");
    let mut most = Grant { names: long[..MOST_NAME].to_owned(), ..oversized };
    most.lookups[0].len = MOST_NAME as u32;
    assert_eq!(Grant::decode(&encoded(&most)), Ok(most));
    assert!(MOST_BINDS * 17 > frame::MOST_BODY as usize, "the frame bounds what a grant carries first");
}

#[test]
fn a_grant_stores_only_into_a_dependency_images_private_copy() {
    let refused = |change: &dyn Fn(&mut Grant), why: &str| {
        let mut grant = a_grant();
        change(&mut grant);
        assert!(grant.check_binds().is_err(), "the daemon made a grant that {why}");
        let error = Grant::decode(&encoded(&grant)).unwrap_err();
        assert!(!error.what.is_empty(), "{why}");
    };
    // Into the arena, the cells, the code, or outside every overlay.
    refused(&|it| it.writes[0].place = 0x10_8000, "writes code");
    refused(&|it| it.writes[0].place = 0x10_c000, "writes the cells");
    refused(&|it| it.writes[0].place = 0x10_0000 + P, "writes the image's code");
    refused(&|it| it.writes[0].place = 0x10_f000, "writes between overlays");
    refused(&|it| it.lookups[0].place = 0x10_0000, "binds a header");
    // Across the end of the copy, or past the top of the address space.
    refused(&|it| it.writes[1].place = 0x11_0000 + 8 * P - 4, "writes past the copy");
    refused(&|it| it.lookups[1].place = u64::MAX - 3, "wraps");
    // A copy not flagged a dependency image's, or the image's data not a copy.
    refused(&|it| it.overlays[3].image = false, "writes a fixed map that is no dependency image");
    refused(&|it| it.overlays[3].access = Access::Read, "writes read-only data");
    refused(&|it| it.overlays[3].access = Access::Execute, "writes code");
    // Names empty, with a NUL, or out of the names.
    refused(&|it| it.lookups[0].len = 0, "binds an empty name");
    refused(&|it| it.names = "mal\0oc__rg_alloc".to_owned(), "binds a name with a NUL");
    refused(&|it| it.lookups[1].len = 11, "binds a name past the names");
    // Only a fixed map is a dependency image: never the code memory, the arena or the cells.
    for overlay in 0..3 {
        let mut grant = a_grant();
        grant.overlays[overlay].image = true;
        assert!(Grant::decode(&encoded(&grant)).is_err(), "overlay {overlay} flagged a dependency image");
    }
    // Flags other than 0 and 1.
    let bytes = encoded(&a_grant());
    let image = writes_at(&a_grant()) - 1;
    let address = writes_at(&a_grant()) + 4 + 16;
    let weak = writes_at(&a_grant()) + 4 + 17 * 2 + 4 + 16;
    for at in [image, address, weak] {
        assert!(bytes[at] <= 1);
        let mut forged = bytes.clone();
        forged[at] = 2;
        assert!(Grant::decode(&forged).is_err(), "a flag of 2 at {at}");
    }
}

/// The run's private copy of the dependency image's data and the memory object it was copied
/// from, as `sys::overlay` leaves them for an `Access::Copy` overlay on every platform: the
/// model where a platform can't be run here.
struct Copied {
    at: u64,
    object: Vec<u8>,
    copy: Vec<u8>,
}

impl Copied {
    fn of(grant: &Grant, slide: u64) -> Copied {
        let map = grant.overlays.iter().find(|it| it.image).unwrap();
        let object: Vec<u8> = (0..map.len).map(|it| it as u8 | 0x80).collect();
        Copied { at: map.at.wrapping_add(slide), copy: object.clone(), object }
    }

    fn store(&mut self, at: u64, value: u64) {
        let offset = (at - self.at) as usize;
        self.copy[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn word(&self, at: u64) -> u64 {
        let offset = (at - self.at) as usize;
        u64::from_le_bytes(self.copy[offset..offset + 8].try_into().unwrap())
    }

    fn untouched(&self, at: u64) -> bool {
        let offset = (at - self.at) as usize;
        self.copy[offset..offset + 8] == self.object[offset..offset + 8]
    }
}

#[test]
fn the_start_stores_writes_and_lookups_in_the_private_copy_and_a_weak_miss_as_zero() {
    let grant = Grant::decode(&encoded(&a_grant())).unwrap();
    let mut copied = Copied::of(&grant, 0);
    let mut asked = Vec::new();
    crate::bind::bind(
        &grant,
        0,
        |name| {
            asked.push(name.to_owned());
            Ok((name == "malloc").then_some(0x7f12_3456_7000))
        },
        |at, value| copied.store(at, value),
    )
    .unwrap();
    assert_eq!(asked, ["malloc", "__rg_alloc"]);
    assert_eq!(copied.word(0x11_0010), 0x11_2000);
    assert_eq!(copied.word(0x11_0000 + 8 * P - 8), 0x40);
    assert_eq!(copied.word(0x11_0020), 0x7f12_3456_7000);
    assert_eq!(copied.word(0x11_0028), 0, "a weak miss is 0, not its addend");
    assert!(copied.untouched(0x11_0030));
    assert_eq!(copied.object, Copied::of(&grant, 0).object, "the memory object never changes");

    // A weak lookup found gets its addend.
    let mut copied = Copied::of(&grant, 0);
    crate::bind::bind(&grant, 0, |_| Ok(Some(0x1000)), |at, value| copied.store(at, value)).unwrap();
    assert_eq!(copied.word(0x11_0028), 0x1000 - 16);
}

#[test]
fn a_strong_miss_fails_the_start_naming_it_and_is_never_stored() {
    let mut grant = a_grant();
    grant.lookups.push(Lookup { place: 0x11_0030, addend: 0, weak: false, name: 0, len: 6 });
    let grant = Grant::decode(&encoded(&grant)).unwrap();
    let mut copied = Copied::of(&grant, 0);
    let error = crate::bind::bind(&grant, 0, |_| Ok(None), |at, value| copied.store(at, value)).unwrap_err();
    assert!(error.what.contains("`malloc`, `malloc`"), "{error}");
    assert!(copied.untouched(0x11_0020) && copied.untouched(0x11_0030), "a strong miss stored");
    assert_eq!(copied.word(0x11_0028), 0, "the weak one is still 0");

    use std::fmt::Write as _;
    let names = (0..12).fold(String::new(), |mut names, it| {
        write!(names, "f{it:02}").unwrap();
        names
    });
    let many = Grant {
        writes: Vec::new(),
        lookups: (0..12).map(|it| Lookup { place: 0x11_0100 + 8 * it as u64, addend: 0, weak: false, name: 3 * it, len: 3 }).collect(),
        names,
        ..a_grant()
    };
    let many = Grant::decode(&encoded(&many)).unwrap();
    let error = crate::bind::bind(&many, 0, |_| Ok(None), |_, _| panic!("stored a strong miss")).unwrap_err();
    assert!(error.what.contains("`f00`") && error.what.contains("`f07`") && !error.what.contains("f08"), "{error}");
    assert!(error.what.contains("and 4 more"), "{error}");

    // What the scope says goes wrong is the start's error.
    let failed = crate::bind::bind(&grant, 0, |_| Err(crate::Error::new("no scope")), |_, _| {});
    assert_eq!(failed.unwrap_err().what, "no scope");
}

#[test]
fn a_slide_is_refused_while_a_dependency_image_is_mapped_and_moves_only_addresses() {
    let grant = a_grant();
    crate::bind::check_slide(&grant, 0).unwrap();
    let error = crate::bind::check_slide(&grant, 0x5555_0000_0000).unwrap_err();
    assert!(error.what.contains("0x110000..0x118000") && error.what.contains("0x555500000000"), "{error}");
    let mut plain = a_grant();
    plain.overlays[3].image = false;
    plain.writes.clear();
    plain.lookups.clear();
    crate::bind::check_slide(&plain, 0x5555_0000_0000).unwrap();

    // Where the rule is lifted, a write's address moves with the slide and an offset doesn't.
    let slide = 0x1_0000;
    let mut copied = Copied::of(&grant, slide);
    crate::bind::bind(&grant, slide, |_| Ok(Some(0x9000)), |at, value| copied.store(at, value)).unwrap();
    assert_eq!(copied.word(0x11_0010 + slide), 0x11_2000 + slide);
    assert_eq!(copied.word(0x11_0000 + 8 * P - 8 + slide), 0x40);
    assert_eq!(copied.word(0x11_0020 + slide), 0x9000, "a found address is the process's own");
}

#[test]
fn lookups_use_the_c_name() {
    use crate::bind::c_name;
    assert_eq!(c_name(Format::Elf, "malloc").unwrap(), "malloc");
    assert_eq!(c_name(Format::Pe, "_malloc").unwrap(), "_malloc");
    assert_eq!(c_name(Format::MachO, "_malloc").unwrap(), "malloc");
    assert_eq!(c_name(Format::MachO, "__rg_alloc").unwrap(), "_rg_alloc");
    assert!(c_name(Format::MachO, "malloc").unwrap_err().what.contains("`malloc`"));
    assert!(c_name(Format::MachO, "_").is_err());

    let mut grant = a_grant();
    grant.format = Format::MachO;
    grant.names = "_malloc__rg_alloc".to_owned();
    grant.lookups[0].len = 7;
    grant.lookups[1].name = 7;
    let mut asked = Vec::new();
    crate::bind::bind(
        &grant,
        0,
        |name| {
            asked.push(name.to_owned());
            Ok(Some(1))
        },
        |_, _| {},
    )
    .unwrap();
    assert_eq!(asked, ["malloc", "_rg_alloc"]);
}

#[test]
fn tokens_are_32_hex_digits_after_the_address() {
    let token: [u8; 16] = core::array::from_fn(|it| (it as u8) * 17);
    assert_eq!(hex(&token), "00112233445566778899aabbccddeeff");
    assert_eq!(unhex(hex(&token).as_bytes()), Some(token));
    assert_eq!(unhex(b"000102030405060708090a0b0c0d0e0f"), Some(core::array::from_fn(|it| it as u8)));
    assert_eq!(unhex(b"000102030405060708090a0b0c0d0e0"), None);
    assert_eq!(unhex(b"000102030405060708090a0b0c0d0eFF"), None);
    assert_eq!(unhex(b"zz0102030405060708090a0b0c0d0e0f"), None);
    let (address, token) = host_var(b"/run/blaze/host.sock\t000102030405060708090a0b0c0d0e0f").unwrap();
    assert_eq!((address, token), ("/run/blaze/host.sock", core::array::from_fn(|it| it as u8)));
    assert!(host_var(b"/run/blaze/host.sock").is_err());
    assert!(host_var(b"\xff\t000102030405060708090a0b0c0d0e0f").is_err());
    assert!(host_var(b"a\t00").is_err());
}

#[test]
fn only_the_instructions_arm_allows_are_changed_running() {
    assert!(crate::is_concurrently_modifiable(0x1400_0010));
    assert!(crate::is_concurrently_modifiable(0x9400_0010));
    assert!(crate::is_concurrently_modifiable(0xd503_201f));
    assert!(crate::is_concurrently_modifiable(0xd503_3fdf));
    assert!(crate::is_concurrently_modifiable(0xd420_0000));
    assert!(crate::is_concurrently_modifiable(0xd400_0001));
    assert!(!crate::is_concurrently_modifiable(0x8b02_0020));
    assert!(!crate::is_concurrently_modifiable(0xd65f_03c0));
    assert!(!crate::is_concurrently_modifiable(0x5400_0000));
}

const SUPPORTED: bool = cfg!(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
));

/// A code memory, or on a platform without one, the check that saying so is what happens.
fn memory(image: u64, arena: u64) -> Option<CodeMemory> {
    match CodeMemory::new("test", image, arena) {
        Ok(memory) => Some(memory),
        Err(error) => {
            assert!(!SUPPORTED, "{error}");
            assert!(error.what.contains("blaze-inproc runs on"), "{error}");
            None
        }
    }
}

#[test]
fn sizes_round_up_to_pages() {
    let Some(memory) = memory(1, 1) else { return };
    let page = memory.page();
    assert!(page.is_power_of_two());
    assert_eq!(memory.image_len(), page);
    assert_eq!(memory.len(), 2 * page);
    assert!(!memory.is_empty());
    assert!(CodeMemory::new("test", 0, 0).is_err());
    assert!(CodeMemory::new("test", u64::MAX, 1).is_err());
    assert!(CodeMemory::with_cells("test", 0, 0, 1).is_err());
}

#[test]
fn writes_and_words_are_bounds_and_alignment_checked() {
    let Some(mut memory) = memory(1, 1) else { return };
    let len = memory.len();
    memory.write(16, b"hello").unwrap();
    assert_eq!(memory.bytes(CodeRange { offset: 16, len: 5 }).unwrap(), b"hello");
    memory.fill(16, 2, 0xcc).unwrap();
    assert_eq!(memory.bytes(CodeRange { offset: 16, len: 5 }).unwrap(), b"\xcc\xccllo");
    assert!(memory.write(len - 2, b"abc").is_err());
    assert!(memory.write(u64::MAX, b"a").is_err());
    assert!(memory.bytes(CodeRange { offset: len, len: 1 }).is_err());
    memory.store_word(64, 0x1122_3344_5566_7788).unwrap();
    assert_eq!(memory.load_word(64), Ok(0x1122_3344_5566_7788));
    assert!(memory.store_word(65, 1).is_err());
    assert!(memory.store_word(len - 4, 1).is_err());
    assert!(memory.load_word(4).is_err());
    memory.publish(CodeRange { offset: 0, len }).unwrap();
    assert!(memory.publish(CodeRange { offset: len, len: 1 }).is_err());
}

#[test]
fn cells_change_only_by_word_and_read_through_their_view() {
    let Ok(mut memory) = CodeMemory::with_cells("cells", 1, 1, 1) else {
        assert!(!SUPPORTED);
        return;
    };
    let cells = memory.cells();
    assert_eq!(cells.offset, memory.image_len() + memory.page());
    assert_eq!(memory.len(), cells.end());
    memory.store_word(cells.offset + 8, 99).unwrap();
    assert_eq!(memory.load_word(cells.offset + 8), Ok(99));
    let view = memory.cell_view().unwrap();
    assert_eq!(view.load(cells.offset + 8), Ok(99));
    assert!(view.load(cells.offset - 8).is_err());
    assert!(view.load(cells.end()).is_err());
    assert!(memory.write(cells.offset, b"x").is_err());
    assert!(memory.bytes(CodeRange { offset: cells.offset, len: 8 }).is_err());
    assert!(memory.store_word(cells.end(), 1).is_err());
}

#[test]
fn instruction_stores_are_aarch64s_alone() {
    let Some(mut memory) = memory(1, 1) else { return };
    let nop = 0xd503_201f;
    if cfg!(target_arch = "aarch64") {
        memory.store_instruction(8, nop).unwrap();
        assert_eq!(memory.bytes(CodeRange { offset: 8, len: 4 }).unwrap(), nop.to_le_bytes());
        assert!(memory.store_instruction(10, nop).is_err());
        assert!(memory.store_instruction(8, 0x8b02_0020).is_err());
    } else {
        assert!(memory.store_instruction(8, nop).is_err());
    }
}

#[test]
fn the_arena_hands_out_aligned_ranges_after_the_image() {
    // Room for a 4096-aligned range past the three smaller ones whatever the page size.
    let Some(mut memory) = memory(1, 3 * 4096) else { return };
    let image = memory.image_len();
    assert!(memory.reserve(0, 16).is_err());
    assert!(memory.reserve(16, 24).is_err());
    let mut taken = Vec::new();
    for align in [16, 64, 256, 4096] {
        let range = memory.reserve(100, align).unwrap();
        assert!(range.offset >= image && range.end() <= memory.len());
        assert!(range.offset.is_multiple_of(align));
        assert!(taken.iter().all(|it: &CodeRange| it.end() <= range.offset || range.end() <= it.offset));
        taken.push(range);
    }
    for range in taken {
        memory.unreserve(range).unwrap();
    }
    let all = memory.reserve(memory.len() - image, 16).unwrap();
    let full = memory.reserve(16, 16).unwrap_err();
    assert!(full.what.contains("arena is full"), "{full}");
    memory.retire(all).unwrap();
    assert_eq!(memory.held(), 0, "with no run process, nothing can be in retired code");
    memory.reserve(16, 16).unwrap();
    assert!(memory.retire(CodeRange { offset: 0, len: 16 }).is_err());
    assert!(memory.unreserve(CodeRange { offset: memory.len(), len: 16 }).is_err());
}

/// An ELF executable for pages of `page`: headers, one page of code at file offset `page`,
/// and reservations for an arena and cells of a page each.
fn an_executable(page: u64) -> Vec<u8> {
    let mut bytes = elf(&[(R, 0, 0, 0x200, 0x200), (RX, page, page, page, page), (0, 0, 4 * page, 0, page), (0, 0, 6 * page, 0, page)]);
    bytes.resize(2 * page as usize, 0);
    bytes
}

/// A code memory holding [`an_executable`], laid out; on Linux alone, whose format the
/// builder writes.
fn laid_out(name: &str) -> Option<CodeMemory> {
    if !cfg!(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))) {
        return None;
    }
    let page = crate::sys::page();
    let mut memory = CodeMemory::with_cells(name, 2 * page, page, page).unwrap();
    memory.write(0, &an_executable(page)).unwrap();
    assert!(!memory.is_pristine());
    memory.lay_out(Format::Elf).unwrap();
    Some(memory)
}

#[test]
fn a_laid_out_image_places_the_arena_and_cells_in_its_reservations() {
    let Some(mut memory) = laid_out("layout") else { return };
    let page = memory.page();
    assert!(memory.is_pristine());
    assert_eq!(memory.layout().unwrap().code, [Segment { offset: page, linked: page, len: page }]);
    assert_eq!(memory.linked(page + 8), Some(page + 8));
    assert_eq!(memory.linked(0), None, "the headers are no code a run reaches");
    let arena = memory.image_len();
    assert_eq!(memory.linked(arena + 16), Some(4 * page + 16));
    assert_eq!(memory.linked(memory.cells().offset), Some(6 * page));
    assert_eq!(memory.offset_of(4 * page + 16), Some(arena + 16));
    assert_eq!(memory.offset_of(6 * page + 8), Some(memory.cells().offset + 8));
    assert_eq!(memory.offset_of(5 * page), None);
    let range = memory.reserve(16, 16).unwrap();
    memory.write(range.offset, b"x").unwrap();
    assert!(memory.is_pristine(), "the arena is not the file");
    memory.store_word(memory.cells().offset, 1).unwrap();
    assert!(memory.is_pristine(), "the cells are not the file");
    memory.store_word(page, 1).unwrap();
    assert!(!memory.is_pristine());

    let mut small = CodeMemory::with_cells("small", 2 * page, 2 * page, page).unwrap();
    small.write(0, &an_executable(page)).unwrap();
    let error = small.lay_out(Format::Elf).unwrap_err();
    assert!(error.what.contains("for the arena"), "{error}");
}

#[test]
fn retired_code_waits_for_the_threads_of_a_live_process() {
    let Some(mut memory) = laid_out("retire") else { return };
    let arena = memory.cells().offset - memory.image_len();
    let pid = std::process::id();
    memory.attach(pid);
    let all = memory.reserve(arena, 16).unwrap();
    memory.retire(all).unwrap();
    assert_eq!(memory.held(), arena);
    // This process's threads never reported to the fence: none of them may have left it.
    assert_eq!(memory.collect(), Ok(0));
    let full = memory.reserve(16, 16).unwrap_err();
    assert!(full.what.contains("held by fences"), "{full}");
    memory.exited(pid);
    assert_eq!(memory.held(), 0);
    memory.reserve(arena, 16).unwrap();
}

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod linux {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::channel::Channel;
    use crate::sys;

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!("bzi-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
    }

    /// A function returning `value`, as bytes.
    fn returning(value: u16) -> Vec<u8> {
        #[cfg(target_arch = "x86_64")]
        {
            let mut code = vec![0xb8];
            code.extend_from_slice(&u32::from(value).to_le_bytes());
            code.push(0xc3);
            code
        }
        #[cfg(target_arch = "aarch64")]
        {
            let mut code = (0x5280_0000u32 | (u32::from(value) << 5)).to_le_bytes().to_vec();
            code.extend_from_slice(&0xd65f_03c0u32.to_le_bytes());
            code
        }
    }

    /// A stub that jumps to the address in the 8-byte word right after it, and where in the
    /// stub that word is. x86-64: `jmp [rip + 0]`; AArch64: `ldr x16, #8`, `br x16`.
    fn stub() -> (Vec<u8>, u64) {
        #[cfg(target_arch = "x86_64")]
        {
            (vec![0xff, 0x25, 0, 0, 0, 0], 6)
        }
        #[cfg(target_arch = "aarch64")]
        {
            let mut code = 0x5800_0050u32.to_le_bytes().to_vec();
            code.extend_from_slice(&0xd61f_0200u32.to_le_bytes());
            (code, 8)
        }
    }

    fn call(address: u64) -> u32 {
        // SAFETY: `address` is the start of code this test wrote whole and published, mapped
        // executable for the rest of the test: a function taking nothing and returning a
        // `u32` in the C ABI (`returning`), or a stub jumping to one (`stub`).
        let function: extern "C" fn() -> u32 = unsafe { std::mem::transmute(address as usize) };
        function()
    }

    #[test]
    fn code_written_here_runs_there_and_a_patch_is_one_store() {
        let mut memory = CodeMemory::new("dual", 1, 1).unwrap();
        let run = memory.object().map_here(Access::Execute).unwrap();
        let (a, b) = (0x200, 0x300);
        let (stub, word) = stub();
        // The word after the stub is 8-aligned.
        let at = 0x108 - word;
        memory.write(a, &returning(42)).unwrap();
        memory.write(b, &returning(43)).unwrap();
        memory.write(at, &stub).unwrap();
        memory.store_word(at + word, run.address() + a).unwrap();
        memory.publish(CodeRange { offset: 0, len: 0x400 }).unwrap();
        assert_eq!(call(run.address() + a), 42);
        assert_eq!(call(run.address() + at), 42);
        memory.store_word(at + word, run.address() + b).unwrap();
        assert_eq!(call(run.address() + at), 43);
        memory.write(a, &returning(44)).unwrap();
        memory.publish(CodeRange { offset: a, len: 16 }).unwrap();
        assert_eq!(call(run.address() + a), 44);
    }

    /// Pages of private memory, then holding bytes read and execute: what the kernel maps of
    /// an executable's code segment.
    struct FileLike {
        at: u64,
        len: usize,
    }

    impl FileLike {
        fn new(len: usize) -> FileLike {
            // SAFETY: a new private anonymous mapping the kernel places; nothing else uses it.
            let at = unsafe {
                libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
            };
            assert_ne!(at, libc::MAP_FAILED);
            FileLike { at: at as u64, len }
        }

        /// Puts `bytes` here and makes the pages read and execute, as the file maps.
        fn seal(&self, bytes: &[u8]) {
            assert_eq!(bytes.len(), self.len);
            // SAFETY: `len` writable bytes of the mapping `new` made, which no thread runs yet.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.at as *mut u8, self.len) };
            // SAFETY: the same mapping, made read and execute before any thread runs it.
            assert_eq!(unsafe { libc::mprotect(self.at as *mut libc::c_void, self.len, libc::PROT_READ | libc::PROT_EXEC) }, 0);
        }

        fn bytes(&self) -> &[u8] {
            // SAFETY: `len` readable bytes at `at`, mapped while `self` lives (by this or, after
            // an overlay, by the memory object's mapping in its place).
            unsafe { std::slice::from_raw_parts(self.at as *const u8, self.len) }
        }
    }

    impl Drop for FileLike {
        fn drop(&mut self) {
            // SAFETY: the pages this made (or the overlay that replaced them), which no thread
            // runs any more: the test joined its thread.
            unsafe { libc::munmap(self.at as *mut libc::c_void, self.len) };
        }
    }

    #[test]
    fn an_overlay_swaps_a_running_threads_code_with_every_byte_equal_and_a_patch_is_one_store_it_sees() {
        let mut memory = CodeMemory::new("swap", 1, 0).unwrap();
        let page = memory.page();
        let (stub, word) = stub();
        let at = 0x108 - word;
        let (a, b) = (0x200, 0x300);
        let image = FileLike::new(page as usize);
        memory.write(a, &returning(7)).unwrap();
        memory.write(at, &stub).unwrap();
        // The stub's word names the run's address of `a`, in the file as in the memory.
        memory.store_word(at + word, image.at + a).unwrap();
        memory.publish(CodeRange { offset: 0, len: page }).unwrap();
        image.seal(memory.bytes(CodeRange { offset: 0, len: page }).unwrap());
        assert_eq!(call(image.at + at), 7);

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let entry = image.at + at;
        let runner = {
            let (stop, calls) = (stop.clone(), calls.clone());
            std::thread::spawn(move || {
                let (mut sevens, mut eights) = (0u64, 0u64);
                while !stop.load(Ordering::Relaxed) || eights == 0 {
                    calls.fetch_add(1, Ordering::Relaxed);
                    match call(entry) {
                        7 => {
                            assert_eq!(eights, 0, "the old code after the patch was seen");
                            sevens += 1;
                        }
                        8 => eights += 1,
                        other => panic!("{other} from code that returns 7 or 8"),
                    }
                }
                (sevens, eights)
            })
        };
        let ran_since = |since: u64, what: &str| {
            blaze_spin::SpinWait::within(what, Duration::from_secs(10), || calls.load(Ordering::Relaxed) > since);
        };
        ran_since(0, "the running thread to call the code");
        let (mut channel, _, _) = connected();
        let granted = [sys::granted(memory.object(), Rights::Code).unwrap()];
        let overlay =
            Overlay { object: 0, offset: 0, len: page, at: 0x1000_0000, access: Access::Execute, place: Place::Code, image: false };
        sys::overlay(&mut channel, &granted, &[overlay], image.at.wrapping_sub(0x1000_0000)).unwrap();
        assert_eq!(image.bytes(), memory.bytes(CodeRange { offset: 0, len: page }).unwrap());
        ran_since(calls.load(Ordering::Relaxed) + 1, "the running thread to call the code through the overlay");

        memory.write(b, &returning(8)).unwrap();
        memory.publish(CodeRange { offset: b, len: 16 }).unwrap();
        memory.store_word(at + word, image.at + b).unwrap();
        assert_eq!(image.bytes()[b as usize..b as usize + returning(8).len()], returning(8)[..], "the run sees the daemon's write");
        let deadline = Instant::now() + Duration::from_secs(10);
        stop.store(true, Ordering::Relaxed);
        while !runner.is_finished() {
            assert!(Instant::now() < deadline, "the running thread never saw the patch");
            std::thread::yield_now();
        }
        let (sevens, eights) = runner.join().unwrap();
        assert!(sevens > 0 && eights > 0);
    }

    fn connected() -> (Channel, Channel, u32) {
        let dir = temp_dir();
        let (listener, address) = sys::Listener::bind(&dir).unwrap();
        let hook = sys::connect(&address).unwrap();
        let (daemon, pid) = listener.accept().unwrap();
        drop(listener);
        let _ = std::fs::remove_dir(&dir);
        (Channel::new(daemon), Channel::new(hook), pid)
    }

    #[test]
    fn the_listener_is_this_users_alone_and_knows_its_peer() {
        let dir = temp_dir();
        let (listener, address) = sys::Listener::bind(&dir).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&dir).unwrap().permissions());
        assert_eq!(mode & 0o077, 0);
        let _hook = sys::connect(&address).unwrap();
        let (_, pid) = listener.accept().unwrap();
        assert_eq!(pid, std::process::id());
        drop(listener);
        assert!(!std::path::Path::new(&address).exists());
        let _ = std::fs::remove_dir(&dir);

        let open = temp_dir();
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(sys::Listener::bind(&open).is_err());
        let _ = std::fs::remove_dir(&open);
    }

    #[test]
    fn frames_cross_the_channel_whole_in_order() {
        let (mut daemon, mut hook, _) = connected();
        let mut rng = SplitMix64::new(0x77_0000_0001);
        let bodies: Vec<Vec<u8>> = (0..64)
            .map(|index| {
                let len = if index % 16 == 0 { 200_000 } else { rng.index(5000) };
                random_bytes(&mut rng, len)
            })
            .collect();
        let sent = bodies.clone();
        let sender = std::thread::spawn(move || {
            for (index, body) in sent.iter().enumerate() {
                hook.send(Kind(0x100 + index as u16), index as u32, body).unwrap();
            }
            hook
        });
        for (index, body) in bodies.iter().enumerate() {
            let frame = daemon.recv().unwrap();
            assert_eq!((frame.kind, frame.seq), (Kind(0x100 + index as u16), index as u32));
            assert_eq!(frame.body, body.as_slice());
        }
        let hook = sender.join().unwrap();
        drop(hook);
        let hung = daemon.recv().unwrap_err();
        assert!(hung.what.contains("hung up"), "{hung}");
    }

    #[test]
    fn a_request_waits_for_its_own_answer() {
        let (mut daemon, mut hook, _) = connected();
        let answerer = std::thread::spawn(move || {
            for _ in 0..3 {
                let (seq, body) = {
                    let frame = daemon.recv().unwrap();
                    (frame.seq, frame.body.to_vec())
                };
                let mut answer = body.clone();
                answer.reverse();
                daemon.send(Kind(0x101), seq, &answer).unwrap();
            }
            let frame = daemon.recv().unwrap();
            let seq = frame.seq;
            daemon.send(Kind(0x101), seq.wrapping_add(1), b"wrong").unwrap();
        });
        for body in [&b"abc"[..], b"", b"hello"] {
            let frame = hook.request(Kind(0x100), body).unwrap();
            let mut want = body.to_vec();
            want.reverse();
            assert_eq!(frame.body, want.as_slice());
        }
        assert!(hook.request(Kind(0x100), b"x").is_err());
        answerer.join().unwrap();
    }

    #[test]
    fn bad_bytes_on_the_channel_are_an_error() {
        let (mut daemon, mut hook, _) = connected();
        let mut header = Header { kind: Kind(1), len: 0, seq: 0 }.encode();
        header[3] = 0x80;
        hook.stream().write_all(&header).unwrap();
        assert!(daemon.recv().is_err());
        let (mut daemon, _hook, _) = connected();
        daemon.set_timeout(Some(Duration::from_millis(50))).unwrap();
        let timed = daemon.recv().unwrap_err();
        assert!(timed.what.contains("timed out"), "{timed}");
    }

    #[test]
    fn a_bound_word_lands_in_the_runs_private_copy_never_the_memory_object() {
        let mut memory = CodeMemory::new("binds", 1, 0).unwrap();
        let page = memory.page();
        let data: Vec<u8> = (0..page).map(|it| it as u8 | 0x80).collect();
        memory.write(0, &data).unwrap();
        let (_daemon, mut hook, _) = connected();
        let granted = sys::granted(memory.object(), Rights::Code).unwrap();
        let at = granted.map_anywhere(&mut hook, Access::Copy).unwrap();
        let grant = Grant {
            objects: vec![
                (memory.object().len(), Rights::Code),
                (P, Rights::Words),
                (P, Rights::Words),
                (memory.object().len(), Rights::Code),
            ],
            overlays: vec![Overlay { object: 3, offset: 0, len: page, at, access: Access::Copy, place: Place::Reserved, image: true }],
            writes: vec![ImageWrite { place: at + 8, value: 0x1234, address: false }],
            lookups: vec![
                Lookup { place: at + 0x13, addend: 4, weak: false, name: 0, len: 6 },
                Lookup { place: at + 0x20, addend: 0, weak: true, name: 6, len: 25 },
            ],
            names: "getpidblaze_inproc_no_such_name".to_owned(),
            ..a_grant()
        };
        grant.check_binds().unwrap();
        let mut scope = sys::Scope::new();
        crate::bind::bind(
            &grant,
            0,
            |name| scope.find(name),
            // SAFETY: `at..at + page` is the private copy just mapped, readable and writable,
            // and `check_binds` put every place 8 bytes inside it; no other thread uses it.
            |at, value| unsafe { sys::store_private(at, value) },
        )
        .unwrap();
        // SAFETY: the private copy, mapped for the rest of the test.
        let copy = unsafe { std::slice::from_raw_parts(at as *const u8, page as usize) };
        let word = |offset: usize| u64::from_le_bytes(copy[offset..offset + 8].try_into().unwrap());
        assert_eq!(word(8), 0x1234);
        assert_eq!(word(0x13), libc::getpid as usize as u64 + 4, "the global scope's getpid, unaligned");
        assert_eq!(word(0x20), 0, "a weak miss");
        assert_eq!(copy[..8], data[..8]);
        assert_eq!(copy[0x28..], data[0x28..]);
        assert_eq!(memory.bytes(CodeRange { offset: 0, len: page }).unwrap(), &data[..], "the memory object changed");

        let mut strong = grant.clone();
        strong.lookups[1].weak = false;
        let error = crate::bind::bind(&strong, 0, |name| scope.find(name), |_, _| {}).unwrap_err();
        assert!(error.what.contains("`blaze_inproc_no_such_name`"), "{error}");
    }

    #[test]
    fn the_hook_gets_code_it_can_run_and_never_write_and_words_it_shares() {
        let mut memory = CodeMemory::new("grant", 1, 1).unwrap();
        memory.write(0, &returning(7)).unwrap();
        memory.publish(CodeRange { offset: 0, len: 16 }).unwrap();
        let (mut daemon, mut hook, pid) = connected();
        let objects = [(memory.object(), Rights::Code), (memory.control(), Rights::Words)];
        sys::send_objects(&mut daemon, pid, b"grant", &objects).unwrap();
        {
            let frame = hook.recv().unwrap();
            assert_eq!((frame.kind, frame.body), (kinds::GRANT, &b"grant"[..]));
        }
        let granted = sys::recv_objects(&mut hook, 2).unwrap();
        assert_eq!(granted[0].len(), memory.object().len());
        assert_eq!(granted[1].len(), memory.control().len());

        assert!(granted[0].map_anywhere(&mut hook, Access::Shared).is_err(), "the image mapped writable shared");
        let code = granted[0].map_anywhere(&mut hook, Access::Read).unwrap();
        // SAFETY: the object's first bytes, mapped readable for the rest of the test.
        assert_eq!(unsafe { std::slice::from_raw_parts(code as *const u8, 6) }, &returning(7)[..6]);

        let control = granted[1].map_anywhere(&mut hook, Access::Shared).unwrap();
        let words = sys::adopt_words(control, granted[1].len(), crate::fence::CONTROL_BYTES).unwrap();
        assert!(crate::fence::Thread::ready(&words));
        assert_eq!(crate::fence::Thread::epoch(&words), memory.fences.epoch());
        memory.announce(41);
        assert_eq!(crate::fence::Thread::announced(&words), 41);
    }

    #[test]
    fn the_hook_takes_its_variable_out_of_the_environment_in_place() {
        use std::ffi::{CStr, CString, c_char};

        let take = |entries: &[&str]| {
            let strings: Vec<CString> = entries.iter().map(|it| CString::new(*it).unwrap()).collect();
            let mut envp: Vec<*const c_char> = strings.iter().map(|it| it.as_ptr()).collect();
            envp.push(std::ptr::null());
            // SAFETY: a null-terminated array of the NUL-terminated `strings`, alive meanwhile.
            let value = unsafe { crate::runtime::take_from(envp.as_mut_ptr()) };
            let left: Vec<String> = envp
                .iter()
                .take_while(|it| !it.is_null())
                // SAFETY: each is one of `strings`, alive.
                .map(|it| unsafe { CStr::from_ptr(*it) }.to_str().unwrap().to_owned())
                .collect();
            (value, left)
        };
        let (value, left) = take(&["A=1", "BLAZE_INPROC=/s\tab", "B=2"]);
        assert_eq!(value.as_deref(), Some(&b"/s\tab"[..]));
        assert_eq!(left, ["A=1", "B=2"]);
        let (value, left) = take(&["BLAZE_INPROCX=1", "BLAZE_INPROC", "B=2"]);
        assert_eq!(value, None);
        assert_eq!(left, ["BLAZE_INPROCX=1", "BLAZE_INPROC", "B=2"]);
        assert_eq!(take(&["BLAZE_INPROC="]), (Some(Vec::new()), Vec::new()));
        // SAFETY: a null `envp` is one the hook may get.
        assert_eq!(unsafe { crate::runtime::take_from(std::ptr::null_mut()) }, None);
    }

    #[test]
    fn this_process_names_its_own_image() {
        let (header, headers) = sys::own_image().unwrap();
        assert_eq!(&headers[..4], b"\x7fELF");
        assert!(header.is_multiple_of(sys::page()));
        let here = this_process_names_its_own_image as usize as u64;
        assert!(here > header, "this test's code is above its executable's header");
    }

    #[test]
    fn a_thread_lists_itself() {
        let mut threads = Vec::new();
        sys::threads(std::process::id(), &mut threads).unwrap();
        assert!(threads.contains(&sys::thread_id()));
        sys::threads(u32::MAX - 1, &mut threads).unwrap();
        assert!(threads.is_empty());
    }

    #[test]
    fn a_backtrace_ends_at_the_outermost_frame() {
        let mut frames = [0u64; crate::fence::MOST_FRAMES];
        let len = sys::backtrace(&mut frames).unwrap();
        assert!(len > 1);
        let mut short = [0u64; 1];
        assert_eq!(sys::backtrace(&mut short), None);
    }

    #[cfg(feature = "host")]
    mod host {
        use std::sync::Arc;

        use blaze_core::{Command, Sandbox};
        use parking_lot::Mutex;

        use super::*;
        use crate::handshake::HOST_VAR;
        use crate::host::{Executable, Host, Image, Launch, Program, Wrote};

        #[test]
        fn an_executable_is_written_once_per_key_and_never_from_a_patched_memory() {
            let Some(mut memory) = laid_out("write") else { return };
            let page = memory.page();
            let dir = temp_dir();
            std::fs::create_dir_all(&dir).unwrap();
            let host = Host::new(&dir.join("host")).unwrap();
            let path = dir.join("prog");
            let len = 2 * page - 3;
            let write = |memory: &CodeMemory, key: u8, store: Option<&std::path::Path>| {
                host.write_executable(memory, &Executable { path: &path, key: [key; 32], len, store })
            };
            let want = memory.bytes(CodeRange { offset: 0, len }).unwrap().to_vec();

            assert_eq!(write(&memory, 1, None), Ok(Wrote::Written));
            assert_eq!(std::fs::read(&path).unwrap(), want);
            let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions());
            assert_eq!(mode & 0o777, 0o755);
            assert_eq!(write(&memory, 1, None), Ok(Wrote::Kept));
            assert_eq!(write(&memory, 2, None), Ok(Wrote::Written));
            std::fs::write(&path, b"another tool's file").unwrap();
            assert_eq!(write(&memory, 2, None), Ok(Wrote::Written), "a file changed since is written again");
            assert_eq!(std::fs::read(&path).unwrap(), want);

            let store = dir.join("store");
            std::fs::write(&store, &want).unwrap();
            let cloned = write(&memory, 3, Some(&store)).unwrap();
            assert!(matches!(cloned, Wrote::Cloned | Wrote::Written), "{cloned:?}");
            assert_eq!(std::fs::read(&path).unwrap(), want);
            std::fs::write(&store, b"not the image").unwrap();
            match write(&memory, 4, Some(&store)) {
                Ok(wrote) => assert_eq!(wrote, Wrote::Written, "no clones here: the bytes"),
                Err(error) => assert!(error.what.contains("not the image"), "{error}"),
            }
            assert_eq!(write(&memory, 5, None), Ok(Wrote::Written));
            assert_eq!(std::fs::read(&path).unwrap(), want);

            memory.store_word(page, 1).unwrap();
            assert_eq!(write(&memory, 5, None), Ok(Wrote::Kept), "the file of this key is there: nothing to write");
            let error = write(&memory, 6, None).unwrap_err();
            assert!(error.what.contains("no longer the file the linker wrote"), "{error}");
            let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|it| it.ok()).filter(|it| it.file_name().to_string_lossy().ends_with(".tmp")).collect();
            assert!(leftovers.is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Plays the start hook of `command` in this process: hello at a slid header, the
        /// grant checked against the image's own layout, the control words and the asks
        /// mapped, STARTED.
        fn play_hook(command: &Command, own: &Layout, slide: u64) -> (Channel, Grant, &'static [std::sync::atomic::AtomicU64]) {
            let var = command.env.iter().find(|(name, _)| name == HOST_VAR).unwrap().1.to_str().unwrap().to_owned();
            let (address, token) = host_var(var.as_bytes()).unwrap();
            let mut channel = Channel::new(sys::connect(address).unwrap());
            let mut hello = Vec::new();
            Hello { token, pid: std::process::id(), version: VERSION, page: sys::page(), header: own.header + slide }.encode(&mut hello);
            channel.send(kinds::HELLO, 0, &hello).unwrap();
            let grant = {
                let frame = channel.recv().unwrap();
                assert_eq!(frame.kind, kinds::GRANT);
                Grant::decode(frame.body).unwrap()
            };
            grant.check(own, sys::page()).unwrap();
            let granted = sys::recv_objects(&mut channel, grant.objects.len()).unwrap();
            let control = granted[1].map_anywhere(&mut channel, Access::Shared).unwrap();
            let control = sys::adopt_words(control, granted[1].len(), crate::fence::CONTROL_BYTES).unwrap();
            assert!(crate::fence::Thread::ready(&control));
            let asks = granted[2].map_anywhere(&mut channel, Access::Shared).unwrap();
            let asks = sys::adopt_words(asks, granted[2].len(), crate::ask::BYTES).unwrap();
            assert!(crate::ask::ready(&asks));
            let mut started = Vec::new();
            Writer::new(&mut started).u64(sys::thread_id());
            channel.send(kinds::STARTED, 0, &started).unwrap();
            (channel, grant, asks)
        }

        #[test]
        fn the_host_grants_the_code_segments_arena_and_cells_and_learns_the_slide() {
            let Some(memory) = laid_out("host") else { return };
            let page = memory.page();
            let dir = temp_dir();
            let host = Host::new(&dir).unwrap();
            let program = dir.join("prog");
            let launch = Launch {
                image: &memory,
                maps: Vec::new(),
                writes: Vec::new(),
                lookups: Vec::new(),
                program: program.clone(),
                args: vec!["a".into(), "b".into()],
                env: vec![("X".into(), "1".into()), (HOST_VAR.into(), "stale".into())],
                dir: dir.clone(),
                stdin: Vec::new(),
                sandbox: Sandbox::On {
                    read: Vec::new(),
                    write: Vec::new(),
                    network: false,
                    trace: false,
                    services: Vec::new(),
                },
                memory: None,
            };
            let (command, pending) = host.launch(&launch).unwrap();
            assert_eq!(command.program, program);
            assert_eq!(command.args, ["a", "b"]);
            assert_eq!(command.env.iter().filter(|(name, _)| name == HOST_VAR).count(), 1);
            assert!(command.env.iter().any(|(name, value)| name == "X" && value == "1"));
            let Sandbox::On { read, write, network, services, .. } = &command.sandbox else {
                panic!("the run left its sandbox")
            };
            assert!(read.contains(&program));
            if cfg!(target_os = "macos") {
                assert_eq!(*services, [format!("dev.blaze.inproc.{}", std::process::id())]);
            } else {
                assert!(services.is_empty());
            }
            assert_eq!(write.len(), 1);
            assert!(!network);

            let slide = 0x5555_0000_0000;
            let own = memory.layout().unwrap().clone();
            let (mut channel, grant, asks) = play_hook(&command, &own, slide);
            assert_eq!(
                grant.objects.iter().map(|it| it.1).collect::<Vec<_>>(),
                [Rights::Code, Rights::Words, Rights::Words, Rights::Words]
            );
            assert_eq!(grant.overlays, [
                Overlay { object: 0, offset: page, len: page, at: page, access: Access::Execute, place: Place::Code, image: false },
                Overlay {
                    object: 0,
                    offset: 2 * page,
                    len: page,
                    at: 4 * page,
                    access: Access::Execute,
                    place: Place::Reserved,
                    image: false,
                },
                Overlay { object: 3, offset: 0, len: page, at: 6 * page, access: Access::Shared, place: Place::Reserved, image: false },
            ]);
            let session = pending.wait(Duration::from_secs(10)).unwrap();
            assert_eq!((session.pid, session.slide, session.thread), (std::process::id(), slide, sys::thread_id()));
            assert_eq!(session.killed(), None);
            let (mut reader, sender) = session.split();
            assert_eq!(sender.pid(), std::process::id());
            let answering = std::thread::spawn(move || {
                let frame = reader.recv().unwrap();
                assert_eq!((frame.kind, frame.body), (Kind(0x300), &b"ask"[..]));
                sender.send(Kind(0x301), frame.seq, b"answer").unwrap();
            });
            let mut answer = [0u8; 16];
            let asked = crate::ask::request(&asks, channel.stream(), Kind(0x300), b"ask", &mut answer).unwrap();
            assert_eq!((asked, &answer[..6]), ((Kind(0x301), 6), &b"answer"[..]));
            answering.join().unwrap();

            let (stale, wrong) = host.launch(&launch).unwrap();
            let var = stale.env.iter().find(|(name, _)| name == HOST_VAR).unwrap().1.to_str().unwrap().to_owned();
            let (address, mut token) = host_var(var.as_bytes()).unwrap();
            token[0] ^= 1;
            let mut channel = Channel::new(sys::connect(address).unwrap());
            let mut hello = Vec::new();
            Hello { token, pid: std::process::id(), version: VERSION, page: sys::page(), header: 0 }.encode(&mut hello);
            channel.send(kinds::HELLO, 0, &hello).unwrap();
            assert!(wrong.wait(Duration::from_millis(300)).is_err());
        }

        #[test]
        fn a_program_attaches_its_processes_until_they_exit_and_never_one_that_ended_first() {
            let Some(memory) = laid_out("program") else { return };
            let page = memory.page();
            let own = memory.layout().unwrap().clone();
            let dir = temp_dir();
            std::fs::create_dir_all(&dir).unwrap();
            let host = Arc::new(Host::new(&dir.join("host")).unwrap());
            let memory = Arc::new(Mutex::new(memory));
            let (sessions, got) = crossbeam_channel::unbounded();
            let image = Image { key: [9; 32], len: 2 * page, store: None };
            let program = Program::new(host, memory.clone(), image, sessions);
            let command = || Command {
                program: dir.join("prog"),
                args: Vec::new(),
                env: Vec::new(),
                dir: dir.clone(),
                stdin: Vec::new(),
                sandbox: Sandbox::Off,
                memory: None,
            };
            let pid = std::process::id();

            let (spawned, started) = program.command(command(), false).unwrap();
            assert_eq!(std::fs::read(dir.join("prog")).unwrap(), an_executable(page));
            let _channel = play_hook(&spawned, &own, 0);
            let session = got.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
            assert_eq!(session.pid, pid);
            assert!(memory.lock().processes.iter().any(|it| it.pid == pid && !it.forks));
            program.exited(started);
            assert!(memory.lock().processes.is_empty());
            program.exited(started);

            let (spawned, started) = program.command(command(), true).unwrap();
            program.exited(started);
            let _channel = play_hook(&spawned, &own, 0);
            let ended = got.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(ended.is_err_and(|it| it.what.contains("ended while")));
            assert!(memory.lock().processes.is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
