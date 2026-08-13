# Migration to `atomic-write-file-xplat`

## Objective

Migrate `atomic-blob-store` from the upstream Unix-only dependency plus its
in-tree Windows save implementation to `atomic-write-file-xplat` 0.1. Preserve
the public API, V1 envelope, platform behavior, lifecycle, cancellation rules,
Windows staging names, stale cleanup, and observable error model wherever the
new dependency provides the underlying operation.

The migration starts only after the fork has satisfied [`PLAN.md`](PLAN.md),
published version `0.1.0`, and demonstrated the Windows behavior in
[`CONTRACT.md`](CONTRACT.md).

## Preserved behavior

The migration must not change:

- `BlockingAtomicBlobStore` or the feature-gated Tokio facade;
- key hashing, configured filename suffixes, namespace layout, or V1 envelope
  bytes and immutable fixtures;
- size validation, streaming chunking, CRC32C validation, or fail-closed reads;
- per-key FIFO scheduling, operation limits, close/flush lifecycle, or accepted
  work's cancellation behavior;
- `.tmp-v1.save.<64-hex>`, `.tmp-v1.clear.<64-hex>`, and
  `.quarantine-v1.<64-hex>` Windows names;
- the age-gated, store-owned Windows cleanup API and `CleanupReport` semantics;
- clear, quarantine, inspection, and their post-rename ambiguity behavior;
- the absence of Tokio from a default blocking-only consumer; or
- the existing public `AtomicBlobStoreError` variants and operation labels.

No Unix or Windows Cargo features will be introduced. Target `cfg` continues to
select platform code.

## 1. Change dependency routing

1. Replace the Unix-only `atomic-write-file` dependency with
   `atomic-write-file-xplat = "0.1"` under
   `cfg(any(unix, windows))` and update the lockfile.
2. Retain `windows-sys`: clear, quarantine, timestamp refresh, owned-file
   deletion, namespace synchronization, and Windows path helpers still need
   native APIs.
3. Retain `getrandom` for store-owned 64-character identifiers used by save,
   clear, and quarantine.
4. Confirm that the packaged crate resolves the released registry dependency,
   not a workspace path or unpinned Git checkout.

Acceptance: blocking and Tokio dependency graphs contain one atomic-write
implementation, and the blocking graph still excludes Tokio.

## 2. Migrate save operations

1. Keep envelope construction and streaming validation in
   `atomic-blob-store`. The fork receives bytes through its `Write`
   implementation and must not know about blob headers, payload lengths, or
   checksums.
2. On Unix, replace imports with the fork's compatible API and retain the
   current writer options, permission behavior, test hooks, and explicit
   namespace synchronization performed by the store.
3. On Windows, generate the current random identifier and exact
   `<canonical>.tmp-v1.save.<identifier>` component. Open the writer with
   `OpenOptions::open_with_temporary_name`. Retry only an `AlreadyExists`
   collision with a newly generated identifier.
4. For complete saves, write header, payload, and checksum to the writer. For
   streaming saves, pass the same writer into the existing bounded receiver and
   envelope-writing path.
5. Invoke `commit_detailed` only after envelope writing and stream completion
   succeed. Continue calling the store's `sync_windows_directory` after a
   successful commit so the migration does not silently weaken existing
   observable behavior.
6. On input, stream, validation, or pre-commit failures, call `discard` when a
   cleanup error can be represented without hiding the primary error. Otherwise
   retain the primary error and rely on Drop plus stale cleanup, matching the
   present best-effort policy.

Acceptance: complete and streaming saves produce byte-identical envelopes,
replace an existing blob old-or-new, and retain the exact documented Windows
staging format.

## 3. Preserve errors and test hooks

1. Map writer-open failures to `OpenAtomicWriter`, envelope writes and staging
   flush failures to `WriteEnvelope`, and detailed replacement failures to the
   existing `AtomicCommit` error.
2. Do not expose `CommitOutcome` through a new public blob-store error in this
   migration. The existing public documentation already treats every atomic
   commit error as ambiguous; preserve that conservative rule even when the
   dependency reports `NotCommitted` internally.
3. Retain the detailed error as the source where the existing error type permits
   it, so diagnostics can identify the Win32 failure without changing matching
   behavior for callers.
4. Keep current test stages around envelope creation, writer open, during
   writing, before commit, simulated commit error, after commit, and namespace
   synchronization. Place hooks in the store wrapper; do not add blob-specific
   hooks to the reusable fork.
5. Preserve the current distinction between a successfully committed rename
   followed by namespace-sync failure and a replacement failure.

Acceptance: existing error-pattern tests pass unchanged, and injected failures
at every stage leave only the states currently permitted by the store.

## 4. Remove only superseded Windows code

After both save paths use the fork:

1. Remove the in-tree Windows staging-file creators and their duplicated
   `FlushFileBuffers` write logic.
2. Remove save-specific direct `MoveFileExW` replacement code and constants.
3. Keep shared wide-path conversion or move it only if all remaining clear,
   quarantine, cleanup, timestamp, deletion, and namespace-sync callers are
   accounted for.
4. Keep `move_file`, `delete_file`, `refresh_windows_clear_age`,
   `sync_windows_directory`, owned-name recognition, and cleanup until a
   separate reviewed change replaces each use.
5. Do not migrate clear or quarantine into the fork: they are blob-store
   lifecycle operations rather than atomic file writing.

Acceptance: no duplicated Windows save/replace implementation remains, while
clear, quarantine, cleanup, and namespace synchronization retain their current
native paths.

## 5. Documentation updates

1. Update crate-level documentation and README to say that both Unix and
   Windows saves use `atomic-write-file-xplat`, with Windows using its native
   exclusive staging and write-through replacement backend.
2. Keep the current old-or-new process-interruption language and hardware,
   network-filesystem, reparse-point, and cross-process limitations.
3. Keep `FORMAT.md` staging names unchanged. Clarify that the store supplies the
   Windows save staging component through the fork's explicit-name API, while
   Unix temporary names remain dependency-owned and unparsed.
4. Add the fork contract and migration completion evidence to `RELEASE.md`.
5. Update `CHANGELOG.md` only if the dependency change alters a user-visible
   platform guarantee or error source; do not claim a format migration.

## 6. Verification and acceptance

Run the repository's full validation set:

```text
cargo check --locked --workspace --all-targets
cargo test --locked --workspace --all-features
cargo test --locked -p atomic-blob-store --no-default-features
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo doc --locked -p atomic-blob-store --no-deps
scripts/validate-atomic-blob-package.sh
python3 scripts/check-markdown-links.py
```

Add or adapt regression coverage for:

- byte-for-byte V1 fixture compatibility on both facades;
- absent and replacement saves through complete and streaming APIs;
- early input, trailing input, cancellation, writer failure, and commit failure;
- exact Windows save staging names and removal by stale cleanup;
- preservation of recent files, unrelated files, clear staging, and per-entry
  cleanup failures;
- child-process interruption before/during writing, before commit, after
  commit, during clear, and after quarantine rename;
- extended-length, relative, and non-Unicode Windows roots;
- existing-reader and sharing-violation behavior inherited from the fork;
- same-key FIFO execution, cross-key concurrency, flush barriers, close, and
  dropped Tokio operation behavior;
- package consumers for blocking-only and Tokio builds; and
- absence of Tokio and unnecessary Windows dependencies from unsupported
  target graphs.

Native Windows release evidence must run on `windows-2022` and, before release,
the manually dispatched `windows-latest` compatibility job. The save
interruption suite must confirm a local NTFS test volume and accept only a
complete old or complete new canonical blob plus staging states documented by
the store.

Migration is complete only when all existing tests and package checks pass,
new dependency-specific regressions pass, immutable fixtures are unchanged,
and a diff review confirms that no public feature or documented Windows staging
behavior was accidentally removed.

