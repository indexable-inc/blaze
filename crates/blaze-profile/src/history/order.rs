//! The order a flush writes its rows in: one total order per table, so the same rows give the
//! same file bytes whichever order the rings were drained in (deterministic output).
//!
//! Until 2026-11-16 a flush wrote `spans` and `counters` in the order [`flight::Read`] held them,
//! which is the order the threads' rings were drained in: a thread registered first on one run
//! and second on the next gave the same rows in another order, other row groups and other
//! dictionary keys (a dictionary column keys each text the first time a row names it), so two
//! runs that recorded the same spans wrote different bytes.
//!
//! Sorted in place: no allocation, `O(n log n)` once a flush over rows the flush already holds.

use crate::flight::{CounterRow, SpanRow};

/// The key a `spans` row is written in: its build, its thread, its start, then its id. The id is
/// unique among one process's spans (the ring hands each span its own), and a flush writes one
/// process's rows, so the key is total and an unstable sort gives one order.
fn span_key(row: &SpanRow) -> (u32, u32, i64, u64) {
    (row.build, row.thread, row.start_ns, row.span_id)
}

/// `spans` in the order a flush writes them ([`span_key`]).
pub(crate) fn spans(spans: &mut [SpanRow]) {
    spans.sort_unstable_by_key(span_key);
}

/// The key a `counters` row is written in: every column it has, so two rows under one key are
/// equal rows and their order changes no byte.
fn counter_key(row: &CounterRow) -> (u32, i64, u8, u32, Option<(u32, u64)>, u64) {
    (row.build, row.t_ns, row.plot as u8, row.named, row.fiber, row.value)
}

/// `counters` in the order a flush writes them ([`counter_key`]).
pub(crate) fn counters(counters: &mut [CounterRow]) {
    counters.sort_unstable_by_key(counter_key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batches::{self, Process};

    fn span(span_id: u64, thread: u32, start_ns: i64) -> SpanRow {
        SpanRow {
            build: 1,
            thread,
            thread_name: 0,
            span_id,
            parent: 0,
            depth: 0,
            site: 0,
            start_ns,
            dur_ns: 1_000 + span_id as i64,
            alloc: None,
            copied: None,
            krate: 0,
            function: 0,
            unit: 0,
            self_cpu_ns: None,
            typed: crate::span::Said::EMPTY.typed,
            folded_spans: 0,
            folded_ns: 0,
        }
    }

    /// The bytes a flush writes of `rows` (as the live feed's Arrow IPC encodes the batch the
    /// history writes), after the flush's ordering.
    fn written(mut rows: std::vec::Vec<SpanRow>) -> std::vec::Vec<u8> {
        spans(&mut rows);
        let process = Process { pid: 9, name: "order test", commit: None };
        let batch = batches::spans(&rows, process).unwrap();
        let mut bytes = std::vec::Vec::new();
        crate::live::encode(&batch, &mut bytes).unwrap();
        bytes
    }

    /// Prints "spans written equal in either drain order: N bytes". One set of spans on two
    /// threads, two of them starting at the same nanosecond (so a key of start alone is not
    /// total), given in the order thread 1's ring then thread 2's, and the reverse: the written
    /// bytes are equal. Without [`spans`] the two batches differ in every per-row column.
    #[test]
    fn spans_drained_in_two_orders_are_written_as_the_same_bytes() {
        let drained = [span(10, 1, 100), span(11, 1, 250), span(20, 2, 100), span(21, 2, 300)];
        let first = written(drained.to_vec());
        let reversed: std::vec::Vec<SpanRow> = drained.iter().rev().copied().collect();
        let second = written(reversed);
        assert!(!first.is_empty(), "a batch of four spans encodes to some bytes");
        assert_eq!(first, second, "the same spans drained in another order are written alike");
        println!("spans written equal in either drain order: {} bytes", first.len());
    }
}
