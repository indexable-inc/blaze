# Query Synchronization (facebook.github.io/watchman/docs/cookies, fetched 2026-10-24), extract

A file system monitor needs to make sure that queries see up-to-date views. Watchman ensures
that by creating a unique cookie for each query made to it.

For watchman, cookies enable us to provide similar guarantees. For a given watchman query:

1. Every file operation that happens before the query is started will be observed.
2. File operations that happen after the query is started may or may not be observed.

A cookie is a temporary file that is created inside a directory observed by watchman.

File monitoring systems like `inotify` typically provide an ordering guarantee: notifications
arrive in the order they happen. Any events happening before the cookie is created will appear
before the event for the cookie does, which means they will be processed by the time the query
is answered.

## Limitation: macOS FSEvents

On macOS, Watchman uses FSEvents to monitor filesystem changes. Watchman uses a combination of
cookie files described above and FSEventStreamFlushSync to attempt to catch up with all prior
changes.

Unfortunately, in high-load situations like a large `git checkout` on a busy host, we have
observed that FSEvents from the `git checkout` may be received after the cookie file
notification and FSEventStreamFlushSync returning.

It turns out that FSEvents provides no guarantees here, and relying on it for query
synchronization is unsupported on any current Apple platform. Their suggested workaround is to
implement a watcher with Endpoint Security. Nobody has evaluated the feasibility of this yet.

In the meantime, you can set a `settle_period` and `settle_timeout` on the query.
