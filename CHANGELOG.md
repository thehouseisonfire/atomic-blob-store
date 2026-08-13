# Changelog

## [Unreleased]

- Add a local-NTFS-qualified Windows evidence suite with native volume reports,
  deterministic save-phase failures, sharing and already-open-handle coverage,
  and reproducible 2,000-attempt CI/10,000-attempt release interruption
  campaigns. Pin the release-evidence toolchain to Rust 1.85.0.
- Clarify that Windows old-or-new replacement behavior is a local-NTFS,
  test-backed engineering expectation rather than a universal Win32 or
  cross-filesystem guarantee, and distinguish namespace flush requests from
  Unix directory `fsync`. Describe the crate as interruption-resistant instead
  of implying universal crash consistency.
- Split the combined license text into separate `LICENSE-MIT` and
  `LICENSE-APACHE` files and record `thehouseisonfire <lefttolive@proton.me>`
  as the copyright holder and crate author. The crate remains dual-licensed
  under MIT OR Apache-2.0.

## [0.1.2] - 2026-07-28

- Exclude `docs/` from the published package so the crate archive stays small;
  the README logo is served from the repository and does not render on crates.io.
- Fix Windows-only compilation errors: import `CleanupFailure` and gate `format` module import to unix tests.

## [0.1.1] - 2026-07-28

- Lower the minimum supported Rust version from 1.89 to 1.85.

## [0.1.0] - 2026-07-27

- Add `BlockingAtomicBlobStore` blocking facade.
- Add optional `tokio::AtomicBlobStore` async facade (feature-gated).
- Add bounded-memory streaming saves (`AsyncWrite`) and loads (`AsyncRead`).
- Add ordered `flush` and deterministic `close` lifecycle operations.
- Add native Windows filesystem backend.


## [0.1.0-alpha.1] - 2026-07-23

- Extract the bounded atomic blob snapshot abstraction with configurable
  identity, bounded coordination, stable format documentation, and neutral API.
