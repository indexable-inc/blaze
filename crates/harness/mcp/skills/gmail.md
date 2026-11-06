---
name: gmail
description: Andrew's Gmail in ts (gmail.*): search and read threads, send, reply, forward, drafts, archive, trash, labels, attachments; read before touching his mail.
---
Andrew's Gmail is `gmail.*` in ts (his logged-in browser, Gmail's own web endpoints): `const t = await
gmail.search('is:unread', { limit: 20 })` gives a Table of threads {id, subject, from, date, snippet, labels,
unread}; `await gmail.read(t[0])` gives {subject, messages} with each message's from, to, date, plain `text`
and attachments; gmail.messages(query), gmail.labels() and gmail.attachment(threadId, msgId, partId) (a saved
path) too. To show mail, draw the rows as Tiles whose onclick calls gmail.read and shows that thread below.
Writing: `await gmail.send({ to: 'a@b.com', subject: 'Hi', body: 'text', attachments: ['/path/f.pdf'] })`;
`const [m] = await gmail.messages('from:bob newer_than:2d'); await gmail.reply(m, 'Thanks!', { all: true })`;
`await gmail.forward(m, 'c@d.com', 'FYI')`; `await gmail.archive(t.filter(x => !x.unread))`. Send, reply,
forward, archive, trash and label ask Andrew first and throw if he declines, so call them directly (no
separate confirm step); gmail.draft(spec) saves without asking, for him to review, and gmail.markRead(rows)
needs no OK.

## The calls

gmail.* works through the harness browser's profile (embedded Chromium the daemon runs), via Gmail's own web
endpoints. Andrew signs in to Google once in the browser pane (cmd+shift+B); until then each call fails saying so.

- via({ cdp: 'http://127.0.0.1:9222' }) points gmail.* (only them, for this kernel's life) at another browser's
  DevTools endpoint instead, as HARNESS_GMAIL_CDP does; via({ cdp: null }) goes back. Never chosen on its own.

- search(query, { limit? = 25 (max 100), page? = 0 }): Promise<Table<{ id: 'thread-f:…'; subject; from; fromEmail;
  date: Date; snippet; labels: string[]; unread; messages: number }>> — Gmail search syntax ('is:unread',
  'from:x newer_than:7d', 'has:attachment').
- messages(query, opts?): Table<{ id: 'msg-f:…'; threadId; subject; from; fromEmail; date; snippet; labels; unread }>.
- read(threadId | row | rows[]): { id; subject; labels; unread; messages: Table<{ id; threadId; subject; from;
  fromEmail; to; cc; bcc; date; labels; unread; messageId; text (plain, quoted history cut); quotedRemoved;
  attachments: Table<{ partId; name; mime; size }> }> } (+ message: the one row when given a message row; an array
  of targets gives a Table, one request).
- labels(): Table<{ id; name; system; query }>.
- attachment(threadId, msgId, partId, { dir? }): Promise<path> — saved under $HARNESS_HOME/gmail/attachments/<msg>/
  by default.

Writing:

- send({ to, cc?, bcc?, subject, body | html, attachments?: paths, replyTo?: message row }) sends in one call.
- reply(messageRow, body, { all?, html?, attachments? }) answers in its thread (quoting it, Re: subject, to the
  sender or with all everyone).
- forward(messageRow, to, note?) resends body and attachments.
- send, reply and forward ask the person first (recipients, subject, body preview) and throw when they decline or
  can't be asked; nothing is sent then.
- draft(spec) saves a draft without asking (→ { id, threadId, ... }); send(draft) sends it, deleteDraft(draft)
  discards it.
- archive(targets), trash(targets), label(targets, { add?, remove? }) (names: Starred, Important, Inbox, user
  labels) ask first; markRead(targets, read? = true) doesn't.

targets: a thread id, a thread row, a message row, or an array of them.
