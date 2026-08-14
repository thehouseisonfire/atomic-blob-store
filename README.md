<div align="center">
  <img alt="atomic-blob-store logo" src="docs/atomic-blob-store.png" width="60%" />
</div>

<div align="center">
  <a href="https://crates.io/crates/atomic-blob-store">
    <img src="https://img.shields.io/crates/v/atomic-blob-store.svg" alt="crates.io version" />
  </a>
  <a href="https://crates.io/crates/atomic-blob-store">
    <img src="https://img.shields.io/crates/d/atomic-blob-store.svg" alt="crates.io downloads" />
  </a>
  <img src="https://img.shields.io/badge/rustc-1.85%2B-blue" alt="rustc 1.85 or newer" />
  <a href="./LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="MIT OR Apache-2.0 license" /></a>
</div>

`atomic-blob-store` saves one size-limited byte blob per opaque key on a trusted
local Unix or Windows filesystem. It supports interruption-resistant streaming
save, validated streaming load, complete-blob conveniences, metadata
inspection, quarantine, clear, and explicit maintenance.

This crate is not a database, queue, object service, log, cache-coherence
protocol, or multi-process coordination mechanism.

The crate is maintained at
<https://github.com/thehouseisonfire/atomic-blob-store>. The default build is
blocking and Tokio-free; enable the optional `tokio` feature for the async
facade.

## Contract

- Keys are arbitrary bytes. Canonical filenames contain the lowercase full
  BLAKE3 digest of the key followed by the configured safe suffix.
- Payloads are complete replacements. Blocking `Read` and feature-gated Tokio
  `AsyncRead` facades accept a declared length and require EOF immediately
  after that length. Data is staged and checksummed incrementally before commit.
- `load_into` validates the entire envelope in constant memory before writing
  any payload bytes, then seeks back and streams the payload into a borrowed
  blocking `Write` or Tokio `AsyncWrite`. This deliberate two-pass read prevents
  corrupt data from reaching the destination.
- Streaming uses fixed-size chunks and bounded channels. Memory does not grow
  with blob size, while replacement and load I/O remain linear; validated
  streaming load reads the payload twice.
- `save(Vec<u8>)` and `load() -> Option<Vec<u8>>` remain conveniences for
  callers that already own or need a complete allocation.
- The default payload limit remains 64 MiB as a safety policy. Applications may
  configure another representable limit.
- One executor-neutral coordinator and a per-store, lazily started bounded
  worker pool serve all clones. The blocking and optional `tokio` facades share
  this engine. Same-key work is FIFO, different keys run concurrently up to the
  configured bound, and caller runtimes never own filesystem work.
- Complete-blob and maintenance operations are submitted when their method is
  called. Streaming operations are submitted when first polled because their
  borrowed endpoints must be actively driven. `flush` waits for all operations
  submitted before its barrier. `close` is an ordered, idempotent lifecycle
  barrier that drains prior work, rejects later work with `StoreClosed`, and
  joins the store-owned workers and coordinator thread.
- Dropping a streaming save before its input-complete marker aborts staging and
  preserves the old canonical blob. After that marker, commit drains and only
  the result is discarded. Dropping a streaming load stops output and releases
  its same-key slot after the blocking worker observes cancellation.
- Independent stores, even at the same path, do not coordinate with each other.
  When all handles are dropped, already submitted work drains best-effort
  before the workers and coordinator exit. Process termination can interrupt
  this; call `close` when a deterministic shutdown point is required.

Reader and writer I/O runs on the caller thread or task. Only bounded byte
chunks cross to the engine, so borrowed endpoints never need to become
`'static`. A slow stream
occupies one configured concurrent-operation slot and applies backpressure.
`load_into` does not flush or shut down its destination. Destination failure or
caller cancellation may leave caller-owned output partially written, but
invalid envelopes produce no output.

## Platform, trust, and durability

The configured root and its ancestors are trusted and application-controlled.
The crate does not defend against hostile path replacement, symlinks, reparse
points, another writer, network filesystems, or storage hardware that violates
filesystem synchronization semantics.

The crate runs on Unix and Windows, but successful operation is not a
certification of every filesystem accepted by those operating systems.

| Windows test root | Support status | Evidence meaning |
| --- | --- | --- |
| Local fixed NTFS volume | Tested contract target | Eligible for process-interruption visibility evidence |
| ReFS or FAT/exFAT | Best-effort compatibility | Passing tests do not substantiate atomic visibility or durability |
| SMB, redirected, or cloud-backed path | Best-effort compatibility | No interruption, coordination, or durability guarantee |
| Container, virtual, or otherwise uncharacterized storage | Configuration-specific | Evidence applies only when the actual test volume is identified and qualified |

CRC32C detects accidental corruption only. There is no authentication,
encryption, tamper resistance, compare-and-swap, transaction support, locking,
lease, fencing, or cross-process guarantee.

The implementation writes and flushes a complete same-directory staging file
before invoking the platform replacement primitive. On native local
filesystems whose rename behavior matches the tested environments, this is
intended to provide a complete old-or-new canonical file under process
interruption; clear is correspondingly intended to provide old-or-absent.

On Windows, the native evidence workflow identifies the volume containing each
actual contract-test root, rejects non-local or non-NTFS roots as release
evidence, exercises deterministic failure phases, and abruptly terminates
uncooperative save loops at randomized times around repeated `MoveFileExW`
calls. The bounded profile performs 2,000 terminations and the manually
dispatched release soak performs 10,000. This is empirical, reproducible local
NTFS evidence—not proof of a universal Win32 guarantee or proof that any
particular termination overlapped the syscall.

Flush and write-through calls request persistence but cannot guarantee survival
under arbitrary power loss or storage that ignores those requests. The Windows
namespace-directory flush is a checked additional operation; Windows does not
document it as a portable equivalent of Unix directory `fsync`.

An atomic commit error is ambiguous: the old complete blob or new complete blob
may be canonical. Reload to determine the observable state. Corrupt,
wrong-domain, future-version, oversized, truncated, and trailing-data envelopes
fail closed and are not modified or automatically quarantined.

A save can also return a namespace-synchronization error after replacement has
already made the complete new blob canonical. Likewise, clear can report a
post-rename synchronization or temporary-file removal error after the canonical
path is already absent. Callers must inspect or reload after these errors rather
than infer state solely from the result.

Quarantine also has an explicit post-rename ambiguity: namespace
synchronization can fail after the canonical path has already moved. In that
case `QuarantineNamespaceSync` carries the committed diagnostic path; the
canonical path is absent and the complete prior blob remains at that path.

Unix uses `atomic-write-file` for same-directory replacement and synchronizes
directories after namespace creation and clear. Dependency-owned temporary
names are never parsed or cleaned by this crate. Windows uses exclusive
same-directory staging files, `FlushFileBuffers`, and native write-through
moves; its explicit cleanup recognizes only names owned by the configured
suffix and store format. Native Windows CI exercises failure boundaries,
randomized process termination during repeated replacement, active data and
executable-image mapping views, unsupported concurrent writers, extended and
non-Unicode paths where the hosted filesystem permits them, both facades, and
independently extracted package consumers. See
[TODO.md](TODO.md) for the evidence runbook and per-release qualification
record.

See [FORMAT.md](FORMAT.md) for the byte-level stable format and compatibility
policy and [RELEASE.md](RELEASE.md) for the independent release-readiness
checklist.

## Facades and features

The default build has no Tokio dependency and exposes
`BlockingAtomicBlobStore`. Enabling the `tokio` feature adds
`atomic_blob_store::tokio::AtomicBlobStore`. Complete-operation methods submit
when called and return an `Operation<T>` that only waits for the result;
borrowed streaming methods submit on first poll. Tokio drives endpoint I/O and
completion waiting only.

Each open store owns one coordinator OS thread. Filesystem workers start lazily
as concurrent work requires them, up to `max_concurrent_operations`, and remain
available until `close` or last-handle drainage. Complete payload submissions
may remain queued with their owned `Vec` allocations; the active-operation
limit does not bound queue count or queued payload memory.

## Test limitation

The test suite intentionally does not claim that concurrent processes or
independently opened stores are safe writers. That behavior is outside the
supported abstraction; applications must enforce a single active owner for a
root/namespace/key tuple.
