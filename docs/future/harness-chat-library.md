---
status: current
summary: Open and search Harness, Claude and Codex conversations with Command-O.
---

# Open any local chat

Command-O opens a searchable chat overlay, separate from the command palette. It includes Harness
chats and local Claude project transcripts and Codex rollouts, including conversations started in
other terminals. Selecting a Harness chat selects its existing object; an external chat opens a
read-only transcript with its source clearly named. Opening never sends a message or takes over an
agent running in another terminal. Refreshing rereads a changed external transcript.

A single `harness-chat-library` library discovers sources and normalizes their records into the
existing chat events. Source paths remain local; discovery reads no credentials. Parsers consume
complete JSONL records and tolerate only an incomplete final record while its writer is active;
malformed complete records report the source and line. Claude branch/session ids and Codex thread
ids identify conversations, not filesystem paths alone. Existing Harness conversations are deduped
by provider conversation id when that id is known.

A worker owns a SQLite FTS5 index under the user's Harness cache. The native UI never scans logs
or runs SQLite on its frame thread. Refresh compares source size and modification time, reparses
changed sources and removes missing sources in one transaction. The index stores bounded searchable
text and metadata, while full transcript events are read only when opened. JSONL and CBOR input
streams record by record, with explicit 4 GiB source and 64 MiB record limits. Schema mismatches and
IO failures are named, never reported as no matches.

Search orders exact ids/titles, id/title prefixes, title matches, then conversation text using
FTS5 BM25 with title/id weighting. Tokenization and query construction treat typed text as data,
not FTS syntax. An empty search lists recently changed chats. Results use a compact two-line list
with a provider mark, a readable title and directory. The selected result has its own preview pane,
rendering the matching excerpt through the transcript's Markdown blocks (headings, lists, links and
fenced code). Excerpt whitespace survives search; preview text never repeats in each result row.
Indexing and keyboard hints sit in the footer. Unknown terms return no matches; an indexing/error
state is distinct. The bounded preview reuses the excerpt already returned by search, with no
additional transcript IO on selection. Its search field shares the chat Draft editor: a real insertion caret,
selection, pointer placement, word movement, clipboard operations and IME composition, with one query value.
The bounded result page is scroll content with the same native physics and overlay scroller as chat inputs;
keyboard selection reveals its row, and the preview scrolls independently. An open overlay consumes input
before the underlying chat or tree. Work remains O(result page + selected excerpt bytes), bounded to forty rows.

Costs: initial discovery O(files + indexed bytes); refresh O(files + changed bytes), query uses FTS
postings instead of rereading conversations. Full transcript parsing is O(selected bytes). The
unavoidable floors are reading new transcript bytes and listing files to discover external writers.
Checks cover real-format fixtures, partial appends, ranked title/id/content queries, refresh and
reopen, plus the scene loader and an offscreen overlay. No agent protocol or source logs change.
