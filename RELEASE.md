# Release readiness

The package remains independently versioned at `0.1.0`. On 2026-07-25,
`cargo search atomic-blob-store` returned no match and `cargo info
atomic-blob-store` reported that the package was absent from the crates.io
index. This is a point-in-time availability check, not a reservation; only a
successful publication reserves a name.

Engineering readiness requires the formatting, feature, test, lint,
documentation, package, dependency, terminology, compatibility-fixture, and
supported-target compile checks documented in this repository. Native platform
evidence is tracked separately and is not inferred from cross-compilation.
Before publication, manually dispatch `.github/workflows/windows.yml`
and require both the pinned `windows-2022` and compatibility
`windows-latest` jobs, including extracted-package consumers, to pass without
ignored failures.

Windows release evidence must record that the actual test root is on a local
NTFS volume. Passing on another filesystem is useful compatibility evidence but
does not substantiate the documented interruption contract. Until the items in
[`TODO.md`](TODO.md) are complete, describe old-or-new behavior during an
interruption overlapping `MoveFileExW` as an engineering expectation rather
than a directly exercised guarantee. A successful Windows namespace-directory
flush must not be represented as a documented equivalent of Unix directory
`fsync`.

Before publication, maintainers must still explicitly accept compatibility,
security-response, documentation, CI, and release-cadence ownership. The
public API and V1 format must also receive feedback from an external,
independent application consumer, with actionable feedback resolved and
affected validation rerun. Publication and external outreach are intentionally
outside this repository change.
