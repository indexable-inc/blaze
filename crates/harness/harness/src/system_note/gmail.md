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
