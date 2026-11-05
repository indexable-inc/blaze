# Why Google Stores Billions of Lines of Code in a Single Repository (CACM 59(7), July 2016), short excerpt

Rachel Potvin and Josh Levenberg. Fetched 2026-10-24 from
https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-single-repository/

> The Google codebase includes approximately one billion files and has a history of
> approximately 35 million commits spanning Google's entire 18-year existence.

> Each day the repository serves billions of file read requests, with approximately 800,000
> queries per second during peak traffic and an average of approximately 500,000 queries per
> second each workday. Most of this traffic originates from Google's distributed build-and-test
> systems.

> Piper and CitC. Piper stores a single large repository and is implemented on top of standard
> Google infrastructure, originally Bigtable, now Spanner.

The CitC part of the article (not in the extract fetched; **reported** from the article): a
cloud-backed file system where a developer's workspace stores only the files they changed, on
top of a view of the repository at a revision, so a workspace costs little and starts instantly.
