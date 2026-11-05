# docs/upstream/ci-runner: what was fetched, from where, when

Fetched on **2026-10-24** for [docs/ci-runner.md](../../ci-runner.md). Files are verbatim.

| file | source | licence | what it backs |
|---|---|---|---|
| `ix.dev-docs-machines.html` | `https://ix.dev/docs/machines` | indexable's, quoted | the shape (100 vCPUs, 256 GiB, 1 PiB), `--cpus`, lifetimes, architecture and CPU class, a fork keeping its architecture |
| `ix.dev-docs-snapshots.html` | `https://ix.dev/docs/snapshots` | indexable's, quoted | a snapshot is memory, disk and devices; restores without limit, each sharing the snapshot's bytes; running processes come with it; what a restore does not honor |
| `ix.dev-docs-sdk.html` | `https://ix.dev/docs/sdk` | indexable's, quoted | `snapshot`, `fork`, `snapshots.restore`, `exec` returning a live `Process` |
| `ix.dev-docs-api.html` | `https://ix.dev/docs/api` | indexable's, quoted | the wire: WebTransport RPC, `vm.snapshot`, `vm.vcs.restore`, `vm.exec` |
| `ix.dev-docs-networking.html` | `https://ix.dev/docs/networking` | indexable's, quoted | private groups, free egress |
| `ix.dev-docs-pricing.html` | `https://ix.dev/docs/pricing` | indexable's, quoted | the meters (CPU time used, memory in use, disk), snapshots billing as unshared disk, machines at once per plan |
| `remote_execution.proto` | `https://raw.githubusercontent.com/bazelbuild/remote-apis/main/build/bazel/remote/execution/v2/remote_execution.proto`, `main` at `c2c07e3d718d` | Apache-2.0 | REAPI v2's services: `ActionCache` has only the unary `GetActionResult` and `UpdateActionResult` (no batch lookup); the CAS batches (`FindMissingBlobs`, `BatchReadBlobs`, `BatchUpdateBlobs`) under a server size limit and streams larger blobs over `ByteStream`; `Execute` needs the command, the action and every input `Directory` uploaded to the CAS first |
| `nrc-grpc-benchmark-README.md` | `https://raw.githubusercontent.com/nrc/grpc-benchmark/master/README.md` (2019) | none stated; quoted for its numbers only | gRPC's own benchmark suite run for Rust (grpc-rs, prost): median latency of an async unary ping-pong 112.3 us, a streaming ping-pong 67-70 us, on a DigitalOcean CPU-optimised droplet |

Not fetched, read through a search (Exa, 2026-10-24) and cited as **reported**: grpc.io's
benchmarking guide (the scenarios), a grpc-io mailing-list thread (C++ unary about 200 us,
Go unary about 300 us on GCE), and LesnyRumcajs/grpc_bench's 2020-08-11 results (tonic about
47k unary requests per second on one server core).

The ix design documents cited in ci-runner.md are in the private `indexable-inc/ix`
repository at `4a647c63ef` (2026-10-24) and are not copied here; ci-runner.md names each path.
