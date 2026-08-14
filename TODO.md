# Remaining Windows contract substantiation work

## Native replacement failure injection

- [x] Replace the pre-call move hooks with a test-only Windows operations seam
  that can return chosen errors from the initial non-replacing and replacing
  `MoveFileExW` calls themselves.
- [x] Exercise documented definitely-not-committed replacement errors and
  conservatively ambiguous errors. For each result, reload through a fresh
  store and assert only the canonical and staging states justified by that
  classification.
- [x] Keep production dispatch static and preserve the public API, V1 envelope,
  staging-name format, and existing error variants.

Acceptance: replacement-error tests model the native call's result rather than
failing immediately before it, and no ambiguous error is treated as proof that
the destination remained unchanged.

## Windows streaming failure-state matrix

- [x] Add local-NTFS-qualified Windows cases for early EOF, trailing input,
  source I/O failure, cancellation during staging writes, cancellation after
  input completion, and failure immediately before commit.
- [x] For each case, assert the returned error category, freshly loaded
  canonical value, and owned staging-file state, including whether age-gated
  cleanup is permitted.
- [x] Cover both blocking and Tokio cancellation semantics where they differ,
  while sharing common state assertions to avoid duplicating equivalent cases.

Acceptance: every streaming error and cancellation boundary has an explicit
Windows state-table assertion rather than relying solely on generic facade
coverage.

## Open-handle and unsupported-writer characterization

- [x] Hold an active data-file mapping created with `MapViewOfFile` while
  replacement is attempted, recording and asserting the supported outcome on
  the qualified runner.
- [x] Hold an active executable-image mapping of a valid image while its
  canonical path is replaced. Record environmental limitations and accept only
  documented sharing/access errors when replacement is rejected.
- [x] Run bounded concurrent negative-contract campaigns with independent store
  instances and a direct external writer targeting the same key. Assert only
  that observed store-produced envelopes are complete and valid; do not assert
  ordering or last-writer behavior.
- [x] Preserve same-store, same-key FIFO coverage as the supported concurrency
  baseline.

Acceptance: evidence distinguishes canonical-name replacement from data held by
existing mappings and from deliberately unsupported writers.

## Native evidence and release qualification

- [x] Run the manually dispatched `windows-2022` 10,000-termination soak and
  retain its artifact URL. Confirm every contract-test environment report
  identifies the actual root as local fixed NTFS and the replacement campaign
  reports completed replacements.
- [x] Run the `windows-latest` 2,000-termination compatibility campaign and
  retain its artifact, exact Rust/Cargo versions, seed, attempt counts, and
  completed-replacement count.
- [x] Confirm the native replacement-error tests, Windows streaming matrix,
  mapping/image tests, standard Windows suite, and extracted-package consumers
  pass in the qualifying workflow.
- [x] Review the resulting artifacts for reproducibility: every failure must
  contain its seed, kill delay, child iteration data, canonical file, owned
  staging files, stdout, stderr, environment report, and reproduction command.
- [x] Record the qualifying artifact links in the release evidence summary and
  align crate docs, README, error documentation, changelog, and release notes
  with the demonstrated boundary.

Acceptance: release wording remains limited to trusted local NTFS, one
coordinated writer, canonical old-or-new visibility under process interruption,
and empirical evidence. It must not imply arbitrary power-loss durability, a
portable Windows equivalent of directory `fsync`, or a universal Win32
guarantee.
