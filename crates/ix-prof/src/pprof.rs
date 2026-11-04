//! pprof's `profile.proto` (github.com/google/pprof/blob/main/proto/profile.proto), written
//! and read by hand: the wire format is small and this keeps the output byte-stable. Gzipped
//! on write, as `go tool pprof` expects; read gzipped or not.

use std::{collections::BTreeMap, io::Read as _, io::Write as _};

use crate::{
    profile::{Frame, FrameKind},
    view::{LabelValue, Sample, ValueType, View},
};

/// The most a gzipped profile may unpack to (256 MiB); a bigger one is refused by name.
pub const MAX_PPROF_BYTES: u64 = 256 << 20;

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pprof: {}", self.0)
    }
}

impl std::error::Error for Error {}

// Writing.

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn key(out: &mut Vec<u8>, field: u32, wire: u32) {
    varint(out, (u64::from(field) << 3) | u64::from(wire));
}

fn int(out: &mut Vec<u8>, field: u32, v: i64) {
    if v != 0 {
        key(out, field, 0);
        varint(out, v as u64);
    }
}

fn bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    key(out, field, 2);
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn packed(out: &mut Vec<u8>, field: u32, vs: impl Iterator<Item = u64>) {
    let mut body = Vec::new();
    for v in vs {
        varint(&mut body, v);
    }
    if !body.is_empty() {
        bytes(out, field, &body);
    }
}

/// Strings in first-use order, `""` first as pprof requires.
#[derive(Default)]
struct Strings {
    ids: BTreeMap<String, i64>,
    list: Vec<String>,
}

impl Strings {
    fn id(&mut self, s: &str) -> i64 {
        if self.list.is_empty() {
            self.list.push(String::new());
            self.ids.insert(String::new(), 0);
        }
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = self.list.len() as i64;
        self.list.push(s.to_owned());
        self.ids.insert(s.to_owned(), id);
        id
    }
}

fn value_type(out: &mut Vec<u8>, field: u32, vt: &ValueType, strings: &mut Strings) {
    let mut body = Vec::new();
    int(&mut body, 1, strings.id(&vt.name));
    int(&mut body, 2, strings.id(&vt.unit));
    bytes(out, field, &body);
}

impl View {
    /// The view as an uncompressed `profile.proto` message.
    pub fn to_pprof_proto(&self) -> Vec<u8> {
        let mut strings = Strings::default();
        strings.id("");
        let mut out = Vec::new();
        for vt in &self.sample_types {
            value_type(&mut out, 1, vt, &mut strings);
        }
        // Mappings: one a module, in name order.
        let mut mappings: BTreeMap<(&str, &str), u64> = BTreeMap::new();
        for f in &self.frames {
            if let Some(module) = &f.module {
                let next = mappings.len() as u64 + 1;
                mappings.entry((module, f.build_id.as_deref().unwrap_or(""))).or_insert(next);
            }
        }
        // Functions: one a (name, system name, file, start line).
        let mut functions: BTreeMap<(&str, Option<&str>, Option<&str>, Option<u32>), u64> =
            BTreeMap::new();
        for f in &self.frames {
            let next = functions.len() as u64 + 1;
            functions
                .entry((&f.function, f.system_name.as_deref(), f.file.as_deref(), f.start_line))
                .or_insert(next);
        }
        // Locations: native frames of one (module, address) share a location, its inline
        // levels its lines; every other frame is a location of its own.
        let mut location_of_frame = vec![0u64; self.frames.len()];
        let mut locations: Vec<(Option<u64>, u64, Vec<usize>)> = Vec::new();
        let mut by_address: BTreeMap<(&str, u64), usize> = BTreeMap::new();
        for (i, f) in self.frames.iter().enumerate() {
            let at = match (f.kind, &f.module, f.address) {
                (FrameKind::Native, Some(module), Some(address)) => {
                    *by_address.entry((module.as_str(), address)).or_insert_with(|| {
                        let mapping = mappings
                            .get(&(module.as_str(), f.build_id.as_deref().unwrap_or("")))
                            .copied();
                        locations.push((mapping, address, Vec::new()));
                        locations.len() - 1
                    })
                }
                _ => {
                    locations.push((None, f.address.unwrap_or(0), Vec::new()));
                    locations.len() - 1
                }
            };
            locations[at].2.push(i);
            location_of_frame[i] = at as u64 + 1;
        }
        for s in &self.samples {
            let mut body = Vec::new();
            // A location stands for all its inline levels: list it once, at its innermost. A
            // frame the run of its location already holds is the location again (a span inside
            // itself, a recursion), listed again: before 2026-11-03 any two equal neighbours
            // were merged, and a stack [f, f] read back as [f].
            let mut ids: Vec<u64> = Vec::with_capacity(s.stack.len());
            let mut run: Vec<u32> = Vec::new();
            for frame in &s.stack {
                let id = location_of_frame[*frame as usize];
                if ids.last() == Some(&id) && !run.contains(frame) {
                    run.push(*frame);
                    continue;
                }
                ids.push(id);
                run.clear();
                run.push(*frame);
            }
            packed(&mut body, 1, ids.into_iter());
            packed(&mut body, 2, s.values.iter().map(|it| *it as u64));
            for (k, v) in &s.labels {
                let mut label = Vec::new();
                int(&mut label, 1, strings.id(k));
                match v {
                    LabelValue::Str(it) => int(&mut label, 2, strings.id(it)),
                    LabelValue::Num(it) => {
                        // A num label of 0 still needs its field to be a num label.
                        key(&mut label, 3, 0);
                        varint(&mut label, *it as u64);
                    }
                }
                bytes(&mut body, 3, &label);
            }
            bytes(&mut out, 2, &body);
        }
        for ((module, build_id), id) in &mappings {
            let mut body = Vec::new();
            int(&mut body, 1, *id as i64);
            int(&mut body, 5, strings.id(module));
            int(&mut body, 6, strings.id(build_id));
            int(&mut body, 7, 1);
            int(&mut body, 8, 1);
            int(&mut body, 9, 1);
            int(&mut body, 10, 1);
            bytes(&mut out, 3, &body);
        }
        for (i, (mapping, address, frames)) in locations.iter().enumerate() {
            let mut body = Vec::new();
            int(&mut body, 1, i as i64 + 1);
            int(&mut body, 2, mapping.unwrap_or(0) as i64);
            int(&mut body, 3, *address as i64);
            // pprof: lines innermost first; our frames: inline_depth 0 is the outermost.
            let mut levels: Vec<&Frame> = frames.iter().map(|it| &self.frames[*it]).collect();
            levels.sort_by_key(|it| std::cmp::Reverse(it.inline_depth));
            for f in levels {
                let mut line = Vec::new();
                let fid = functions[&(
                    f.function.as_str(),
                    f.system_name.as_deref(),
                    f.file.as_deref(),
                    f.start_line,
                )];
                int(&mut line, 1, fid as i64);
                int(&mut line, 2, i64::from(f.line.unwrap_or(0)));
                int(&mut line, 3, i64::from(f.column.unwrap_or(0)));
                bytes(&mut body, 4, &line);
            }
            bytes(&mut out, 4, &body);
        }
        for ((name, system_name, file, start_line), id) in &functions {
            let mut body = Vec::new();
            int(&mut body, 1, *id as i64);
            int(&mut body, 2, strings.id(name));
            int(&mut body, 3, strings.id(system_name.unwrap_or(name)));
            int(&mut body, 4, strings.id(file.unwrap_or("")));
            int(&mut body, 5, i64::from(start_line.unwrap_or(0)));
            bytes(&mut out, 5, &body);
        }
        let period =
            self.period.as_ref().map(|(vt, p)| (strings.id(&vt.name), strings.id(&vt.unit), *p));
        let default = self.sample_types.get(self.default_type).map(|it| strings.id(&it.name));
        // The string table goes last so every id above is in it.
        let mut tail = Vec::new();
        int(&mut tail, 9, self.time_nanos);
        int(&mut tail, 10, self.duration_nanos);
        if let Some((name, unit, p)) = period {
            let mut body = Vec::new();
            int(&mut body, 1, name);
            int(&mut body, 2, unit);
            bytes(&mut tail, 11, &body);
            int(&mut tail, 12, p);
        }
        if let Some(default) = default {
            int(&mut tail, 14, default);
        }
        for s in &strings.list {
            bytes(&mut out, 6, s.as_bytes());
        }
        out.extend_from_slice(&tail);
        out
    }

    /// The view as a gzipped `profile.proto`, what `go tool pprof` reads.
    pub fn to_pprof(&self) -> Vec<u8> {
        let proto = self.to_pprof_proto();
        let mut gz =
            flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::default());
        gz.write_all(&proto).expect("writing to a Vec does not fail");
        gz.finish().expect("writing to a Vec does not fail")
    }

    /// Reads a pprof profile, gzipped or not. Gunzipped, it may be at most
    /// [`MAX_PPROF_BYTES`]: a few bytes of gzip can stand for gigabytes.
    pub fn from_pprof(data: &[u8]) -> Result<View, Error> {
        let raw;
        let proto = if data.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(data)
                .take(MAX_PPROF_BYTES + 1)
                .read_to_end(&mut out)
                .map_err(|it| Error(format!("gunzip: {it}")))?;
            if out.len() as u64 > MAX_PPROF_BYTES {
                return Err(Error(format!(
                    "the gzipped profile is over {MAX_PPROF_BYTES} bytes unpacked: refused (gunzip it and pass the file)"
                )));
            }
            raw = out;
            &raw[..]
        } else {
            data
        };
        decode(proto)
    }
}

// Reading.

struct Wire<'a> {
    buf: &'a [u8],
    at: usize,
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

impl<'a> Wire<'a> {
    fn new(buf: &'a [u8]) -> Wire<'a> {
        Wire { buf, at: 0 }
    }

    fn varint(&mut self) -> Result<u64, Error> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self
                .buf
                .get(self.at)
                .ok_or_else(|| Error("a varint runs past the end".to_owned()))?;
            self.at += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err(Error("a varint is longer than 10 bytes".to_owned()))
    }

    fn next(&mut self) -> Result<Option<(u32, Value<'a>)>, Error> {
        if self.at >= self.buf.len() {
            return Ok(None);
        }
        let k = self.varint()?;
        let field = (k >> 3) as u32;
        let value = match k & 7 {
            0 => Value::Varint(self.varint()?),
            1 => {
                self.skip(8)?;
                Value::Fixed
            }
            2 => {
                let len = self.varint()? as usize;
                let start = self.at;
                self.skip(len)?;
                Value::Bytes(&self.buf[start..start + len])
            }
            5 => {
                self.skip(4)?;
                Value::Fixed
            }
            w => {
                return Err(Error(format!(
                    "field {field} has wire type {w}, which pprof does not use"
                )));
            }
        };
        Ok(Some((field, value)))
    }

    fn skip(&mut self, n: usize) -> Result<(), Error> {
        if self.buf.len() - self.at < n {
            return Err(Error("a field runs past the end".to_owned()));
        }
        self.at += n;
        Ok(())
    }
}

/// A repeated integer, packed or not.
fn ints(v: &Value<'_>, into: &mut Vec<u64>) -> Result<(), Error> {
    match v {
        Value::Varint(it) => into.push(*it),
        Value::Bytes(b) => {
            let mut w = Wire::new(b);
            while w.at < b.len() {
                into.push(w.varint()?);
            }
        }
        Value::Fixed => return Err(Error("a repeated integer is fixed-width".to_owned())),
    }
    Ok(())
}

fn as_int(v: &Value<'_>) -> u64 {
    match v {
        Value::Varint(it) => *it,
        _ => 0,
    }
}

fn message<'a>(
    b: &'a [u8],
    mut each: impl FnMut(u32, &Value<'a>) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut w = Wire::new(b);
    while let Some((field, value)) = w.next()? {
        each(field, &value)?;
    }
    Ok(())
}

#[derive(Default)]
struct RawLocation {
    mapping: u64,
    address: u64,
    lines: Vec<(u64, i64, i64)>,
}

fn decode(proto: &[u8]) -> Result<View, Error> {
    let mut strings: Vec<String> = Vec::new();
    let mut sample_types: Vec<(i64, i64)> = Vec::new();
    let mut samples: Vec<(Vec<u64>, Vec<i64>, Vec<(i64, Option<i64>, Option<i64>)>)> = Vec::new();
    let mut mappings: BTreeMap<u64, (i64, i64)> = BTreeMap::new();
    let mut locations: BTreeMap<u64, RawLocation> = BTreeMap::new();
    let mut functions: BTreeMap<u64, (i64, i64, i64, i64)> = BTreeMap::new();
    let (mut time, mut duration, mut period, mut default) = (0i64, 0i64, 0i64, 0i64);
    let mut period_type: Option<(i64, i64)> = None;
    message(proto, |field, v| {
        match (field, v) {
            (1, Value::Bytes(b)) => {
                let (mut t, mut u) = (0, 0);
                message(b, |f, v| {
                    match f {
                        1 => t = as_int(v) as i64,
                        2 => u = as_int(v) as i64,
                        _ => {}
                    }
                    Ok(())
                })?;
                sample_types.push((t, u));
            }
            (2, Value::Bytes(b)) => {
                let (mut locs, mut vals, mut labels) = (Vec::new(), Vec::new(), Vec::new());
                message(b, |f, v| {
                    match f {
                        1 => ints(v, &mut locs)?,
                        2 => {
                            let mut raw = Vec::new();
                            ints(v, &mut raw)?;
                            vals.extend(raw.into_iter().map(|it| it as i64));
                        }
                        3 => {
                            if let Value::Bytes(lb) = v {
                                let (mut k, mut s, mut n) = (0i64, None, None);
                                message(lb, |f, v| {
                                    match f {
                                        1 => k = as_int(v) as i64,
                                        2 => s = Some(as_int(v) as i64),
                                        3 => n = Some(as_int(v) as i64),
                                        _ => {}
                                    }
                                    Ok(())
                                })?;
                                labels.push((k, s, n));
                            }
                        }
                        _ => {}
                    }
                    Ok(())
                })?;
                samples.push((locs, vals, labels));
            }
            (3, Value::Bytes(b)) => {
                let (mut id, mut file, mut build) = (0, 0, 0);
                message(b, |f, v| {
                    match f {
                        1 => id = as_int(v),
                        5 => file = as_int(v) as i64,
                        6 => build = as_int(v) as i64,
                        _ => {}
                    }
                    Ok(())
                })?;
                mappings.insert(id, (file, build));
            }
            (4, Value::Bytes(b)) => {
                let mut id = 0;
                let mut loc = RawLocation::default();
                message(b, |f, v| {
                    match (f, v) {
                        (1, _) => id = as_int(v),
                        (2, _) => loc.mapping = as_int(v),
                        (3, _) => loc.address = as_int(v),
                        (4, Value::Bytes(lb)) => {
                            let (mut func, mut line, mut col) = (0u64, 0i64, 0i64);
                            message(lb, |f, v| {
                                match f {
                                    1 => func = as_int(v),
                                    2 => line = as_int(v) as i64,
                                    3 => col = as_int(v) as i64,
                                    _ => {}
                                }
                                Ok(())
                            })?;
                            loc.lines.push((func, line, col));
                        }
                        _ => {}
                    }
                    Ok(())
                })?;
                locations.insert(id, loc);
            }
            (5, Value::Bytes(b)) => {
                let (mut id, mut name, mut sys, mut file, mut start) = (0u64, 0, 0, 0, 0);
                message(b, |f, v| {
                    match f {
                        1 => id = as_int(v),
                        2 => name = as_int(v) as i64,
                        3 => sys = as_int(v) as i64,
                        4 => file = as_int(v) as i64,
                        5 => start = as_int(v) as i64,
                        _ => {}
                    }
                    Ok(())
                })?;
                functions.insert(id, (name, sys, file, start));
            }
            (6, Value::Bytes(b)) => strings.push(String::from_utf8_lossy(b).into_owned()),
            (9, _) => time = as_int(v) as i64,
            (10, _) => duration = as_int(v) as i64,
            (11, Value::Bytes(b)) => {
                let (mut t, mut u) = (0, 0);
                message(b, |f, v| {
                    match f {
                        1 => t = as_int(v) as i64,
                        2 => u = as_int(v) as i64,
                        _ => {}
                    }
                    Ok(())
                })?;
                period_type = Some((t, u));
            }
            (12, _) => period = as_int(v) as i64,
            (14, _) => default = as_int(v) as i64,
            _ => {}
        }
        Ok(())
    })?;
    let s = |i: i64| -> Result<String, Error> {
        strings
            .get(usize::try_from(i).map_err(|_| Error(format!("string index {i} is negative")))?)
            .cloned()
            .ok_or_else(|| Error(format!("string index {i} is past the table's {}", strings.len())))
    };
    let nonempty =
        |i: i64| -> Result<Option<String>, Error> { s(i).map(|it| (!it.is_empty()).then_some(it)) };
    let mut view = View { time_nanos: time, duration_nanos: duration, ..View::default() };
    for (t, u) in &sample_types {
        view.sample_types.push(ValueType { name: s(*t)?, unit: s(*u)? });
    }
    if default != 0 {
        let name = s(default)?;
        view.default_type = view.sample_types.iter().position(|it| it.name == name).unwrap_or(0);
    } else {
        view.default_type = view.sample_types.len().saturating_sub(1);
    }
    if let Some((t, u)) = period_type {
        view.period = Some((ValueType { name: s(t)?, unit: s(u)? }, period));
    }
    // Each location becomes its frames, outermost inline level first; remember their ids.
    let mut frames_of_location: BTreeMap<u64, Vec<u32>> = BTreeMap::new();
    for (id, loc) in &locations {
        let (module, build_id) = match mappings.get(&loc.mapping) {
            Some((file, build)) => (nonempty(*file)?, nonempty(*build)?),
            None => (None, None),
        };
        let mut ids = Vec::new();
        let levels = loc.lines.len();
        // pprof lists lines innermost first.
        for (depth, (func, line, col)) in loc.lines.iter().rev().enumerate() {
            let (name, sys, file, start) = functions.get(func).copied().unwrap_or((0, 0, 0, 0));
            let function = s(name)?;
            let system_name = nonempty(sys)?.filter(|it| *it != function);
            let kind = if module.is_some() { FrameKind::Native } else { FrameKind::Span };
            view.frames.push(Frame {
                function: if function.is_empty() {
                    format!("{:#x}", loc.address)
                } else {
                    function
                },
                system_name,
                file: nonempty(file)?,
                line: (*line > 0).then_some(*line as u32),
                column: (*col > 0).then_some(*col as u32),
                start_line: (start > 0).then_some(start as u32),
                module: module.clone(),
                build_id: build_id.clone(),
                address: (loc.address != 0).then_some(loc.address),
                inline_depth: depth as u16,
                kind,
            });
            ids.push(view.frames.len() as u32 - 1);
        }
        if levels == 0 {
            let mut f = Frame::named(format!("{:#x}", loc.address), FrameKind::Native);
            f.address = Some(loc.address);
            f.module = module;
            f.build_id = build_id;
            view.frames.push(f);
            ids.push(view.frames.len() as u32 - 1);
        }
        frames_of_location.insert(*id, ids);
    }
    for (locs, values, labels) in samples {
        let mut stack = Vec::new();
        for loc in locs {
            let ids = frames_of_location.get(&loc).ok_or_else(|| {
                Error(format!("a sample names location {loc}, which is not in the profile"))
            })?;
            // Innermost first: the deepest inline level first.
            stack.extend(ids.iter().rev().copied());
        }
        let mut out_labels = Vec::new();
        for (k, sv, nv) in labels {
            let value = match (sv, nv) {
                (Some(sv), _) if sv != 0 => LabelValue::Str(s(sv)?),
                (_, Some(nv)) => LabelValue::Num(nv),
                _ => LabelValue::Str(String::new()),
            };
            out_labels.push((s(k)?, value));
        }
        view.samples.push(Sample { stack, values, labels: out_labels });
    }
    view.canonicalize();
    Ok(view)
}
