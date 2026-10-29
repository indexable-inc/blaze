//! A call of a function from outside any body: a const macro's expansion
//! (docs/future/const-macros.md). Its arguments are the input tokens as text (stage 0) or as a
//! flat syntax tree read through `const-ast`'s views (stage 2), and its output is read back as
//! text.

use hir_def::FunctionId;

use super::{
    Address, Evaluator, Heap, Interval, IntervalOrOwned, MirEvalError, Result, word_bytes,
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
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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
    db: &'db dyn HirDatabase,
    func: FunctionId,
    args: &[MacroArg<'_>],
) -> Result<'db, String> {
    let body =
        db.runtime_mir_body(func.into()).map_err(|e| MirEvalError::MirLowerError(func, e))?;
    let mut evaluator = Evaluator::new(db, func.into(), false, None)?;
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
fn fat(addr: Address, len: usize) -> Vec<u8> {
    let mut bytes = addr.to_bytes().to_vec();
    bytes.extend_from_slice(&word_bytes(len));
    bytes
}

/// `text` in the heap, as a `&str`.
fn write_str<'db>(evaluator: &mut Evaluator<'db>, text: &str) -> Result<'db, Vec<u8>> {
    Ok(fat(allocate(evaluator, text.as_bytes(), 1)?, text.len()))
}

/// `input` in the heap, as a `&const_ast::Item`: its arrays, the `Tree` of them (`repr(C)`,
/// four slices), and the `Item` (`repr(transparent)` over a `Node`, `repr(C)`: a reference to
/// the tree and the element's number), and the reference to that.
fn write_item<'db>(evaluator: &mut Evaluator<'db>, input: &FlatInput) -> Result<'db, Vec<u8>> {
    let len = input.kinds.len();
    if len == 0
        || [input.ends.len(), input.spans.len(), input.text_ends.len()].iter().any(|&it| it != len)
    {
        return Err(MirEvalError::InternalError(
            "a const macro's flat tree has arrays of different lengths".into(),
        ));
    }
    let ptr_size = evaluator.ptr_size();
    let kinds: Vec<u8> = input.kinds.iter().flat_map(|it| it.to_le_bytes()).collect();
    let ends: Vec<u8> = input.ends.iter().flat_map(|it| it.to_le_bytes()).collect();
    let spans: Vec<u8> = input.spans.iter().flat_map(|it| it.to_le_bytes()).collect();
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
        texts.extend(fat(text.offset(from), to - from));
        start = end;
    }
    let mut tree = fat(allocate(evaluator, &kinds, 2)?, len);
    tree.extend(fat(allocate(evaluator, &ends, 4)?, len));
    tree.extend(fat(allocate(evaluator, &spans, 4)?, len));
    tree.extend(fat(allocate(evaluator, &texts, ptr_size)?, len));
    let tree = allocate(evaluator, &tree, ptr_size)?;
    let mut item = tree.to_bytes().to_vec();
    item.extend_from_slice(&input.item.to_le_bytes());
    item.resize(2 * ptr_size, 0);
    Ok(allocate(evaluator, &item, ptr_size)?.to_bytes().to_vec())
}

/// The text of a `String` (or anything laid out as one: a pointer to its bytes, its capacity
/// and its length, in whichever order) that the evaluator holds at `value`.
fn read_string<'db>(evaluator: &Evaluator<'db>, value: Interval) -> Result<'db, String> {
    let ptr_size = evaluator.ptr_size();
    if value.size != 3 * ptr_size {
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
        words.iter().map(|&it| Address::from_usize(it)).find(|it| matches!(it, Heap(_)))
    else {
        // An empty `String` holds a dangling pointer, not one into the heap.
        return Ok(String::new());
    };
    // The capacity is at least the length.
    let len = words
        .iter()
        .copied()
        .filter(|&it| !matches!(Address::from_usize(it), Heap(_)))
        .min()
        .unwrap_or(0);
    let text = evaluator.read_memory(addr, len)?;
    String::from_utf8(text.to_vec()).map_err(|_| {
        MirEvalError::UndefinedBehavior(
            "a const macro returned a `String` that is not UTF-8".into(),
        )
    })
}
