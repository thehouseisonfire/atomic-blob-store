# Windows contract substantiation TODO

This checklist closes the gap between the Windows operations implemented by
`atomic-blob-store` and the evidence needed to state its interruption contract
confidently. It supplements the existing deterministic tests that stop
immediately before and after save replacement, clear rename, and quarantine
rename.

## 1. Make the test environment part of the evidence

- [ ] Add a Windows test helper that resolves the volume containing the actual
  test directory and reports volume type, filesystem name, drive type, Windows
  build, and whether the path is local. Use native volume APIs rather than
  inferring NTFS from a drive letter.
- [ ] Make contract tests fail or explicitly skip with a recorded reason unless
  the test root is a local NTFS volume. Compatibility tests may still run on
  other filesystems, but their results must not be labeled atomicity evidence.
- [ ] Include the resolved test-root evidence in the Windows CI artifact rather
  than recording only workspace and runner-wide volume listings.
- [ ] Keep `windows-2022` required and run `windows-latest` before release. Pin
  the Rust toolchain used for release evidence or record its exact version.

Acceptance: every interruption artifact identifies the exact filesystem used,
and CI cannot accidentally treat an SMB, ReFS, FAT/exFAT, or redirected test
directory as NTFS contract evidence.

## 2. Exercise interruption overlapping replacement

- [ ] Add a child-process stress harness with no cooperative pause around
  `MoveFileExW`. Initialize a valid old envelope, then have the child repeatedly
  save alternating, uniquely identifiable payloads to one key while the parent
  kills it at randomized high-frequency times.
- [ ] After every kill, open a fresh store and require the canonical path to be
  absent only when it was initially absent; otherwise require a fully valid
  envelope containing exactly one previously committed payload or the current
  complete candidate. Reject truncation, checksum failure, trailing bytes,
  mixed payloads, and unexpected file types.
- [ ] Run separate create and replace campaigns. Replacement is the critical
  contract path; creation ensures the absent-to-present transition is covered.
- [ ] Use payloads spanning zero length, one byte, the streaming chunk
  boundary, several chunks, and a practical large-file case. Include complete
  and streaming save APIs and both facades across the campaign, without
  multiplying every case unnecessarily.
- [ ] Record the random seed, parent kill delay, child iteration counter,
  canonical file, owned staging files, stdout, stderr, and environment report
  for every failure. Make campaigns exactly reproducible from the artifact.
- [ ] Run enough bounded repetitions to make the syscall-overlap window
  credible in CI, and provide a longer manually dispatched soak profile for
  release qualification. Report attempts and observed completed replacements;
  do not claim that timing alone proves every kill overlapped the syscall.

Acceptance: thousands of abrupt terminations on local NTFS expose only complete
old/new envelopes, with reproducible evidence and at least one campaign that
demonstrably made progress through repeated replacement calls.

## 3. Add deterministic failure-state coverage

- [ ] Refactor the Windows save wrapper behind test-only injectable operations
  or hooks so tests can fail staging creation, each write region, staging flush,
  initial non-replacing move, replacing move, and post-move namespace flush.
- [ ] For every injected pre-replacement failure, assert that the old canonical
  blob is unchanged and that any owned staging file is either absent or a state
  explicitly allowed by the public cleanup contract.
- [ ] For every injected replacement failure, assert only states justified by
  the returned Win32 error. Treat unproven cases as ambiguous and reload before
  deciding whether a retry is safe.
- [ ] Test `ERROR_SHARING_VIOLATION` with a destination handle opened without
  `FILE_SHARE_DELETE`. Assert prompt error return, no hidden retry, preservation
  of a valid canonical blob, and age-gated cleanup of abandoned staging.
- [ ] Inject namespace-directory open and flush failures after a successful
  replacement. Assert that save returns `SyncNamespaceDirectory`, while a fresh
  load observes the complete new blob. Document this post-commit error
  ambiguity alongside quarantine's existing explicit form.
- [ ] Add equivalent streaming cases for early EOF, trailing input, input I/O
  failure, cancellation during writing, cancellation after input completion,
  and failure immediately before commit.

Acceptance: each externally observable error class has a test-backed state
table, and no test equates a returned error with an unchanged destination unless
the operation phase establishes that fact.

## 4. Validate open-handle and concurrency boundaries

- [ ] Hold a read handle opened with `FILE_SHARE_DELETE`, replace the blob, and
  demonstrate that the old handle can retain the old file object while a fresh
  open sees the new canonical file.
- [ ] Hold incompatible read, write, mapping, and executable-image handles where
  the runner permits them. Record and assert the supported sharing-violation
  behavior without promising identical error codes for unsupported cases.
- [ ] Stress independent store instances and external writers targeting the
  same key only as a negative contract test. Verify that the documentation
  clearly excludes cross-process coordination rather than treating last-writer
  behavior as a guarantee.
- [ ] Retain same-process, same-store FIFO tests to show that strengthening
  platform tests does not weaken the actual supported concurrency model.

Acceptance: tests visibly distinguish canonical-name atomicity from contents
seen through already-open handles and from unsupported concurrent writers.

## 5. Investigate namespace synchronization honestly

- [ ] Isolate `sync_windows_directory` in native tests and record behavior on
  supported Windows/NTFS versions, including returned errors and required
  access. Treat this as compatibility characterization, not proof of durable
  namespace persistence.
- [ ] Add a test proving that directory-flush failure is surfaced and cannot be
  mistaken for an atomic replacement failure.
- [ ] Review authoritative Microsoft documentation periodically. Unless it
  explicitly supports the operation and guarantee, retain wording that the
  directory flush is an additional checked request, not a Windows equivalent of
  Unix directory `fsync`.
- [ ] Do not add destructive power-cut testing to ordinary CI. If VM or physical
  power-failure testing is later introduced, document the hypervisor, virtual
  disk cache mode, filesystem, storage hardware, and recovery procedure, and
  present the result only for that configuration.

Acceptance: tests and documentation make no durability inference stronger than
the underlying API documentation and recorded environment support.

## 6. Documentation and release gate

- [ ] Replace the README's prose support boundary with a concise Windows support
  matrix after the native filesystem helper exists: local NTFS as the tested
  contract target and other filesystems as best-effort compatibility only.
- [ ] Keep crate-level docs, README, release checklist, and error documentation
  aligned on three separate concepts: complete staging, canonical-name
  visibility, and persistence requests.
- [ ] Link test artifacts or a release evidence summary from `RELEASE.md` without
  claiming that stress testing proves a universal Win32 guarantee.
- [ ] Require the environment check, deterministic failure suite, replacement
  stress campaign, standard Windows suite, and extracted-package consumers for
  the release that promotes the stronger evidence.
- [ ] Revisit the wording only after those gates pass. Even then, scope the
  guarantee to local NTFS, trusted directories, no concurrent writer, and
  process interruption—not arbitrary power loss.

Acceptance: every public guarantee maps to an implemented operation, an
authoritative platform statement where available, and native evidence within
the stated scope.
