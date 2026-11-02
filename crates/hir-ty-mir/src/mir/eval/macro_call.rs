//! A call of a function from outside any body: a const macro's expansion
//! (docs/future/const-macros.md). Its arguments are the input tokens as text (stage 0) or as a
//! flat syntax tree read through `const-ast`'s views (stage 2), and its output is read back as
//! text.

use base_db::Db;
use hir_def::FunctionId;

use super::{
    Address, Evaluator, Heap, Interval, IntervalOrOwned, MirEvalError, OwnedBytes, Result,
    word_bytes,
};
use crate::db::HirDatabase;

/// An argument of a const macro's function.
#[derive(Clone, Copy)]
pub enum MacroArg<'a> {
    /// A `&str`.
    Str(&'a str),
    /// A `&const_ast::Item`: the item of a flat tree.
    Item(&'a FlatInput),
}

/// A flat syntax tree as `const_ast::Tree` holds it, and the element of the item in it.
///
/// Element `i` has the kind `kinds[i]` (with `const_ast::TOKEN` set for a token), ends at
/// `ends[i]`, has the span index `spans[i]`, and, if a token, the text
/// `text[text_ends[i - 1]..text_ends[i]]` (from 0 for element 0).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FlatInput {
    pub kinds: Vec<u16>,
    pub ends: Vec<u32>,
    pub spans: Vec<u32>,
    pub text: String,
    pub text_ends: Vec<u32>,
    pub item: u32,
}

/// Calls the function `func`, whose parameters are `args`, and gives the `String` it
/// returned. `func` is not generic.
pub fn call_const_macro<'db>(
    db: &'db Db,
    func: FunctionId,
    args: &[MacroArg<'_>],
) -> Result<'db, String> {
    let body =
        db.runtime_mir_body(func.into()).map_err(|e| MirEvalError::MirLowerError(func, e))?;
    let mut evaluator = Evaluator::new(db, func.into(), None)?;
    evaluator.compile_time = true;
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        let value = match arg {
            MacroArg::Str(text) => write_str(&mut evaluator, text)?,
            MacroArg::Item(input) => write_item(&mut evaluator, input)?,
        };
        values.push(IntervalOrOwned::Owned(value));
    }
    let result = evaluator.interpret_mir(body, values.into_iter(), None)?;
    read_string(&evaluator, result)
}

/// `bytes` written to a new allocation of the heap aligned to `align`.
fn allocate<'db>(
    evaluator: &mut Evaluator<'db>,
    bytes: &[u8],
    align: usize,
) -> Result<'db, Address> {
    let addr = evaluator.heap_allocate(bytes.len().max(1), align)?;
    evaluator.write_memory(addr, bytes)?;
    Ok(addr)
}

/// A slice's two words: where it starts and its length.
fn fat(addr: Address, len: usize) -> OwnedBytes {
    let mut bytes = OwnedBytes::from_slice(&addr.to_bytes());
    bytes.extend_from_slice(&word_bytes(len));
    bytes
}

/// `text` in the heap, as a `&str`.
fn write_str<'db>(evaluator: &mut Evaluator<'db>, text: &str) -> Result<'db, OwnedBytes> {
    Ok(fat(allocate(evaluator, text.as_bytes(), 1)?, text.len()))
}

/// `input` in the heap, as a `&const_ast::Item`: its arrays, the `Tree` of them (`repr(C)`,
/// four slices), and the `Item` (`repr(transparent)` over a `Node`, `repr(C)`: a reference to
/// the tree and the element's number), and the reference to that.
fn write_item<'db>(evaluator: &mut Evaluator<'db>, input: &FlatInput) -> Result<'db, OwnedBytes> {
    let len = input.kinds.len();
    if len == 0
        || [input.ends.len(), input.spans.len(), input.text_ends.len()].iter().any(|&it| it != len)
    {
        return Err(MirEvalError::InternalError(
            "a const macro's flat tree has arrays of different lengths".into(),
        ));
    }
    let ptr_size = evaluator.ptr_size();
    let text = allocate(evaluator, input.text.as_bytes(), 1)?;
    let mut texts = Vec::with_capacity(len * 2 * ptr_size);
    let mut start = 0;
    for &end in &input.text_ends {
        let (from, to) = (start as usize, end as usize);
        if from > to || to > input.text.len() {
            return Err(MirEvalError::InternalError(
                "a const macro's flat tree has a token past its text".into(),
            ));
        }
        texts.extend_from_slice(&fat(text.offset(from), to - from));
        start = end;
    }
    // The arrays' bytes, each built in `scratch` just before it is allocated.
    let mut scratch = Vec::with_capacity(len * 4);
    scratch.extend(input.kinds.iter().flat_map(|it| it.to_le_bytes()));
    let mut tree = Vec::with_capacity(8 * ptr_size);
    tree.extend_from_slice(&fat(allocate(evaluator, &scratch, 2)?, len));
    scratch.clear();
    scratch.extend(input.ends.iter().flat_map(|it| it.to_le_bytes()));
    tree.extend_from_slice(&fat(allocate(evaluator, &scratch, 4)?, len));
    scratch.clear();
    scratch.extend(input.spans.iter().flat_map(|it| it.to_le_bytes()));
    tree.extend_from_slice(&fat(allocate(evaluator, &scratch, 4)?, len));
    tree.extend_from_slice(&fat(allocate(evaluator, &texts, ptr_size)?, len));
    let tree = allocate(evaluator, &tree, ptr_size)?;
    let mut item = OwnedBytes::from_slice(&tree.to_bytes());
    item.extend_from_slice(&input.item.to_le_bytes());
    item.resize(2 * ptr_size, 0);
    Ok(OwnedBytes::from_slice(&allocate(evaluator, &item, ptr_size)?.to_bytes()))
}

/// The text of a `String` (or anything laid out as one: a pointer to its bytes, its capacity
/// and its length, in whichever order) that the evaluator holds at `value`.
fn read_string<'db>(evaluator: &Evaluator<'db>, value: Interval) -> Result<'db, String> {
    let ptr_size = evaluator.ptr_size();
    if value.size != 3 * ptr_size {
        // alloc: error path
        return Err(MirEvalError::NotSupported(format!(
            "a const macro returns `String`; its function returned {} bytes",
            value.size
        )));
    }
    let bytes = value.get(evaluator)?;
    let mut words = Vec::with_capacity(3);
    for word in bytes.chunks(ptr_size) {
        words.push(from_bytes!(usize, word));
    }
    let Some(addr) =
        words.iter().map(|&it| Address::from_usize(it)).find(|it| matches!(it.at(), Heap(_)))
    else {
        // An empty `String` holds a dangling pointer, not one into the heap.
        return Ok(String::new()); // alloc: empty, allocates nothing
    };
    // The capacity is at least the length.
    let len = words
        .iter()
        .copied()
        .filter(|&it| !matches!(Address::from_usize(it).at(), Heap(_)))
        .min()
        .unwrap_or(0);
    let text = evaluator.read_memory(addr, len)?;
    String::from_utf8(text.to_vec()).map_err(|_| { // alloc: the macro's output, returned
        MirEvalError::UndefinedBehavior(
            "a const macro returned a `String` that is not UTF-8".into(),
        )
    })
}
