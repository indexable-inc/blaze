//! A call of a function from outside any body: a const macro's expansion
//! (docs/future/const-macros.md). Each argument is a flat syntax tree read through `const-ast`'s
//! views (`&const_ast::Item` or `&const_ast::TokenTree`), and the output, a
//! `const_ast::Output`, is read back from the evaluator's memory as the four arrays it is, with
//! every length bounded: the bytes are user code's and are trusted for nothing.

use base_db::Db;
use hir_def::FunctionId;

use super::{
    Address, Evaluator, Interval, IntervalOrOwned, MirEvalError, OwnedBytes, Result, word_bytes,
};
use crate::db::HirDatabase;

/// A flat syntax tree as `const_ast::Tree` holds it, and the element a view of it starts at
/// (the item for `&const_ast::Item`, the token tree for `&const_ast::TokenTree`).
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

/// A const macro's output as `const_ast::Built` holds it, read out of the evaluator.
///
/// Element `i` has the kind `kinds[i]` (a node kind, `const_ast::TOKEN` for a written token,
/// `const_ast::SPLICE` with the input's number for a splice), its subtree is the `sizes[i]`
/// elements from `i`, `spans[i]` is a token's span index or a splice's element, and its text
/// is `text[text_ends[i - 1]..text_ends[i]]` (from 0 for element 0). `entry` is
/// `const_ast::Entry`'s number. Nothing in it is checked but the bounds [`read_output`]
/// states: the host checks the tree.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FlatOutput {
    pub kinds: Vec<u16>,
    pub sizes: Vec<u32>,
    pub spans: Vec<u32>,
    pub text: String,
    pub text_ends: Vec<u32>,
    pub entry: u8,
}

/// The most elements an output may have, written or after its splices are copied.
pub const OUTPUT_ELEMENTS: usize = 1 << 22;

/// The most bytes of text an output's tokens may have.
pub const OUTPUT_TEXT: usize = 1 << 26;

/// How many `const_ast::Entry`s there are: an entry is below it.
const ENTRIES: u8 = 5;

/// Calls the function `func`, whose parameters are `inputs` (parameter `i` is input `i`, the
/// `Tree::input` its splices name), and gives the `const_ast::Output` it returned. `func` is
/// not generic.
pub fn call_const_macro<'db>(
    db: &'db Db,
    func: FunctionId,
    inputs: &[&FlatInput],
) -> Result<'db, FlatOutput> {
    let body =
        db.runtime_mir_body(func.into()).map_err(|e| MirEvalError::MirLowerError(func, e))?;
    let mut evaluator = Evaluator::new(db, func.into(), None)?;
    evaluator.compile_time = true;
    let mut values = Vec::with_capacity(inputs.len()); // alloc: one word list per call
    for (ordinal, input) in (0u32..).zip(inputs) {
        values.push(IntervalOrOwned::Owned(write_item(&mut evaluator, input, ordinal)?));
    }
    let result = evaluator.interpret_mir(body, values.into_iter(), None)?;
    read_output(&evaluator, result)
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
fn fat(addr: Address, len: usize, word: usize) -> OwnedBytes {
    let mut bytes = OwnedBytes::from_slice(&addr.to_bytes(word));
    bytes.extend_from_slice(&word_bytes(len, word));
    bytes
}

/// `input` in the heap, as a reference to a view of element `input.item`: its arrays, the
/// `Tree` of them (`repr(C)`, four slices then `ordinal`, its `input`: nine words), and the
/// view (`repr(transparent)` over a `Node`, `repr(C)`: a reference to the tree and the
/// element's number), and the reference to that.
fn write_item<'db>(
    evaluator: &mut Evaluator<'db>,
    input: &FlatInput,
    ordinal: u32,
) -> Result<'db, OwnedBytes> {
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
    // alloc: the texts' slices, written once into the evaluator
    let mut texts = Vec::with_capacity(len * 2 * ptr_size);
    let mut start = 0;
    for &end in &input.text_ends {
        let (from, to) = (start as usize, end as usize);
        if from > to || to > input.text.len() {
            return Err(MirEvalError::InternalError(
                "a const macro's flat tree has a token past its text".into(),
            ));
        }
        texts.extend_from_slice(&fat(text.offset(from), to - from, ptr_size));
        start = end;
    }
    // The arrays' bytes, each built in `scratch` just before it is allocated.
    let mut scratch = Vec::with_capacity(len * 4); // alloc: one array's bytes at a time
    scratch.extend(input.kinds.iter().flat_map(|it| it.to_le_bytes()));
    let mut tree = Vec::with_capacity(9 * ptr_size); // alloc: the `Tree`'s nine words
    tree.extend_from_slice(&fat(allocate(evaluator, &scratch, 2)?, len, ptr_size));
    scratch.clear();
    scratch.extend(input.ends.iter().flat_map(|it| it.to_le_bytes()));
    tree.extend_from_slice(&fat(allocate(evaluator, &scratch, 4)?, len, ptr_size));
    scratch.clear();
    scratch.extend(input.spans.iter().flat_map(|it| it.to_le_bytes()));
    tree.extend_from_slice(&fat(allocate(evaluator, &scratch, 4)?, len, ptr_size));
    tree.extend_from_slice(&fat(allocate(evaluator, &texts, ptr_size)?, len, ptr_size));
    tree.extend_from_slice(&ordinal.to_le_bytes());
    tree.resize(9 * ptr_size, 0);
    let tree = allocate(evaluator, &tree, ptr_size)?;
    let mut item = OwnedBytes::from_slice(&tree.to_bytes(ptr_size));
    item.extend_from_slice(&input.item.to_le_bytes());
    item.resize(2 * ptr_size, 0);
    Ok(OwnedBytes::from_slice(&allocate(evaluator, &item, ptr_size)?.to_bytes(ptr_size)))
}

/// The `const_ast::Output` the evaluator holds at `value`: `repr(C)`, four slices (kinds,
/// sizes, spans, texts) then the entry's byte, nine words. Every array is read through
/// [`Evaluator::read_memory`], which refuses memory the program does not have; the arrays are
/// as long as each other and at most [`OUTPUT_ELEMENTS`], all text at most [`OUTPUT_TEXT`]
/// bytes and UTF-8, and the entry below [`ENTRIES`], or the call is refused saying which.
fn read_output<'db>(evaluator: &Evaluator<'db>, value: Interval) -> Result<'db, FlatOutput> {
    let word = evaluator.ptr_size();
    if value.size != 9 * word {
        // alloc: error path
        return Err(MirEvalError::NotSupported(format!(
            "a const macro returns `const_ast::Output`; its function returned {} bytes",
            value.size
        )));
    }
    let bytes = value.get(evaluator)?;
    // The four slices' words: `(address, length)` of kinds, sizes, spans and texts.
    let mut slices = [(Address::from_usize(0, word), 0usize); 4];
    for (n, slice) in slices.iter_mut().enumerate() {
        let at = n * 2 * word;
        let addr = from_bytes!(usize, word, &bytes[at..at + word]);
        let len = from_bytes!(usize, word, &bytes[at + word..at + 2 * word]);
        *slice = (Address::from_usize(addr, word), len);
    }
    let entry = bytes[8 * word];
    let len = slices[0].1;
    if slices.iter().any(|&(_, it)| it != len) || len > OUTPUT_ELEMENTS {
        // alloc: error path
        return Err(MirEvalError::UndefinedBehavior(
            format!(
                "a const macro's output has arrays of {}, {}, {} and {} elements (kinds, sizes, \
                 spans, texts); they are as long as each other and at most {OUTPUT_ELEMENTS}",
                slices[0].1, slices[1].1, slices[2].1, slices[3].1
            )
            .into(),
        ));
    }
    if entry >= ENTRIES {
        // alloc: error path
        return Err(MirEvalError::UndefinedBehavior(
            format!("a const macro's output has the entry {entry}; an entry is 0 to 4").into(),
        ));
    }
    // alloc: the output's arrays, handed to the expander, which drops them once it built the tree
    let mut output = FlatOutput {
        kinds: Vec::with_capacity(len),
        sizes: Vec::with_capacity(len),
        spans: Vec::with_capacity(len),
        text: String::new(),
        text_ends: Vec::with_capacity(len),
        entry,
    };
    if len == 0 {
        return Ok(output);
    }
    let u32_of = |it: &[u8]| u32::from_le_bytes([it[0], it[1], it[2], it[3]]);
    let kinds = evaluator.read_memory(slices[0].0, len * 2)?;
    output.kinds.extend(kinds.chunks_exact(2).map(|it| u16::from_le_bytes([it[0], it[1]])));
    let sizes = evaluator.read_memory(slices[1].0, len * 4)?;
    output.sizes.extend(sizes.chunks_exact(4).map(u32_of));
    let spans = evaluator.read_memory(slices[2].0, len * 4)?;
    output.spans.extend(spans.chunks_exact(4).map(u32_of));
    let texts = evaluator.read_memory(slices[3].0, len * 2 * word)?;
    for (element, text) in texts.chunks_exact(2 * word).enumerate() {
        let addr = from_bytes!(usize, word, &text[..word]);
        let size = from_bytes!(usize, word, &text[word..]);
        if size > OUTPUT_TEXT - output.text.len() {
            // alloc: error path
            return Err(MirEvalError::UndefinedBehavior(
                format!(
                    "a const macro's output has more than {OUTPUT_TEXT} bytes of text, at \
                     element {element}"
                )
                .into(),
            ));
        }
        if size != 0 {
            let bytes = evaluator.read_memory(Address::from_usize(addr, word), size)?;
            let Ok(text) = std::str::from_utf8(bytes) else {
                // alloc: error path
                return Err(MirEvalError::UndefinedBehavior(
                    format!("the text of element {element} of a const macro's output is not UTF-8")
                        .into(),
                ));
            };
            output.text.push_str(text);
        }
        output.text_ends.push(output.text.len() as u32);
    }
    Ok(output)
}
