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

Windows release evidence must come from the manually dispatched
[native Windows workflow](.github/workflows/windows.yml) and retain its
`atomic-blob-store-windows-2022-<run>-<attempt>` artifact URL in the release
notes. The artifact must show that every actual contract-test root resolved to
a local fixed NTFS volume, record Rust 1.85.0's exact `rustc -Vv`, and contain:

- the deterministic native failure and open-handle suite;
- the 10,000-attempt replacement/create interruption soak with seed, delays,
  completed replacements, environment reports, and any reproduction bundles;
- the ordinary debug/release, blocking/Tokio, Clippy, documentation, and
  extracted-package consumer results; and
- a successful `windows-latest` 2,000-attempt compatibility campaign.

Passing on another filesystem remains useful compatibility evidence but does
not substantiate the interruption contract. Stress timing does not prove that
every kill overlapped `MoveFileExW`, and a successful Windows
namespace-directory flush is an additional checked request, not a documented
equivalent of Unix directory `fsync`.

## 2026-08-13 Windows qualification evidence

Workflow [31758613077](https://github.com/thehouseisonfire/atomic-blob-store/actions/runs/31758613077)
passed at commit `0e0878e55988223e8972bf342b3115322a97dbf4` on both runners.
The retained artifacts are
[windows-2022](https://github.com/thehouseisonfire/atomic-blob-store/actions/runs/31758613077/artifacts/9203950620)
and
[windows-latest](https://github.com/thehouseisonfire/atomic-blob-store/actions/runs/31758613077/artifacts/9203846474).

- All 80 contract-test environment reports identified a local fixed NTFS root.
- `windows-2022` completed 10,000 termination attempts (2,000 create and 8,000
  replace), observing 2,271 completed replacements.
- `windows-latest` completed 2,000 attempts (400 create and 1,600 replace),
  observing 647 completed replacements.
- Both used seed `6840335469060923678`, Rust 1.85.0
  (`4d91de4e48198da2e33413efdcd9cd2cc0c46688`), and Cargo 1.85.0
  (`d73d2caf9 2024-12-31`).
- The deterministic native suite, streaming state matrix, mapping and external
  writer characterization, debug/release and feature suites, Clippy, docs, and
  extracted blocking/Tokio consumers passed on both runners.

Before publication, maintainers must still explicitly accept compatibility,
security-response, documentation, CI, and release-cadence ownership. The
public API and V1 format must also receive feedback from an external,
independent application consumer, with actionable feedback resolved and
affected validation rerun. Publication and external outreach are intentionally
outside this repository change.
