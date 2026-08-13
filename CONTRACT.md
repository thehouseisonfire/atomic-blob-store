# Windows contract for `atomic-write-file-xplat`

This document defines the intended Windows contract for
`atomic-write-file-xplat`, an independent fork of `atomic-write-file`. Normative
terms such as **must**, **must not**, **should**, and **may** describe behavior
that the implementation and its tests are required to uphold.

## Supported environment

The Windows backend supports Windows 10, Windows Server 2016, and newer
versions supported by the crate's Rust toolchain. Its atomic-visibility and
durability contract applies only to ordinary local NTFS volumes.

The backend may operate on ReFS, FAT, exFAT, SMB shares, cloud-synchronized
directories, virtual filesystems, and redirected storage, but that use is
best-effort. The crate does not reject those filesystems at runtime and makes no
atomicity or durability guarantee for them. A future release may promote a
filesystem to supported status only after native conformance and interruption
testing equivalent to the NTFS suite.

The destination directory and its ancestors must be trusted and controlled by
the application. The crate does not defend against reparse-point substitution,
directory replacement, hostile namespace mutation, or another process writing
the same destination concurrently.

## Atomic writer lifecycle

Opening an `AtomicWriteFile` creates a new staging file in the destination's
directory. The destination is not opened for truncation and remains unchanged
until commit.

On Windows, staging creation must use `CreateFileW` with:

- `CREATE_NEW`, so an existing name is never reused or truncated;
- write access, plus read access when requested through `OpenOptions`;
- no sharing while the staging handle is live;
- `FILE_ATTRIBUTE_NORMAL | FILE_FLAG_WRITE_THROUGH`;
- a same-directory name selected randomly by the crate or supplied through the
  validated explicit-name API.

Relative paths must be resolved without losing the original destination
directory. Win32 calls must receive NUL-terminated UTF-16 extended paths. The
implementation must preserve non-Unicode Windows path units and support local
extended-length and UNC paths accepted by the host filesystem.

The normal writer traits and accessors retain upstream behavior: writes, reads,
seeks, and metadata operations address the staging file. A cloned underlying
file handle must be closed before commit on Windows. Because the staging file is
opened without sharing, a live clone can prevent replacement and produce a
sharing violation; the clone is outside the atomic lifecycle.

### Explicit staging names

The fork adds:

```rust
options.open_with_temporary_name(destination, temporary_name)
```

`temporary_name` must be exactly one non-empty normal path component. Absolute
paths, prefixes, `.` and `..`, separators, and names that resolve outside the
destination directory must be rejected with `InvalidInput`. The file must
still be created with `CREATE_NEW`; `AlreadyExists` is returned to let a caller
generate another name. This API changes naming only and must not weaken any
creation, synchronization, or commit guarantee.

## Commit

The Windows commit sequence is:

1. Call `FlushFileBuffers` on the staging handle after all caller writes.
2. Close the writer's staging handle before the namespace operation.
3. Move the staging path over the destination with `MoveFileExW` and
   `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`.

The staging file and destination must be in the same directory and therefore
on the same volume. The backend must not enable cross-volume copy fallback.
The backend must not implement replacement as delete-then-rename.

On a supported NTFS volume, in the absence of concurrent namespace mutation, a
process interruption may leave the canonical destination naming either the
complete old file or the complete new file. The canonical name must not expose
a file partially written by this crate.

Atomic visibility does not mean compare-and-swap or multi-writer coordination.
If multiple writers target one destination, the last successful namespace
operation may win. Users requiring serialization must provide it outside this
crate.

### Durability of success

A successful commit means:

- all writes to the staging file completed;
- `FlushFileBuffers` succeeded for the staging file; and
- the same-volume write-through replacement completed successfully.

These operations request persistence through Windows and the storage stack.
Success is not a universal guarantee against arbitrary power loss, defective
filesystems, virtual-disk behavior, controller caches, device firmware, or
hardware that does not honor flush requests.

The fork does not flush a Windows directory handle and makes no claim that its
success is equivalent to Unix directory `fsync`. Microsoft documents how to
obtain directory handles but does not document `FlushFileBuffers` as a
generally supported directory-handle operation. Applications may perform
additional namespace synchronization, but it is outside this crate's contract.

## Commit errors and recovery

The upstream-compatible method remains:

```rust
pub fn commit(self) -> std::io::Result<()>;
```

Callers that need recovery information use:

```rust
pub fn commit_detailed(self) -> Result<(), CommitError>;

pub enum CommitOutcome {
    NotCommitted,
    Unknown,
}
```

`CommitError` must expose its `CommitOutcome`, underlying `io::Error`, and the
staging path. It must implement `std::error::Error`. The compatibility method
converts the detailed failure into an `io::Error` whose source retains the
detailed error for downcasting.

Failures before the replacement syscall, including staging writes and staging
flush, have outcome `NotCommitted`. The destination is unchanged, although a
staging file may remain. A replacement failure may be classified
`NotCommitted` only when the Win32 result and documented error semantics
establish that the destination was not replaced. All other replacement
failures must use `Unknown`.

After `Unknown`, callers must inspect or reopen the destination before retrying.
They must not assume that failure means the old file is canonical.

| Failure point | Canonical destination | Possible staging state |
| --- | --- | --- |
| Before staging creation | Old file or absent | None |
| During staging write | Old file or absent | Incomplete file may remain |
| Staging flush | Old file or absent | Complete or incomplete file may remain |
| Before replacement | Old file or absent | Complete staged file may remain |
| Replacement, `NotCommitted` | Old file or absent | Complete staged file may remain |
| Replacement, `Unknown` | Complete old or complete new file | May remain or may have been consumed |
| Successful commit | Complete new file | No staging name |

The implementation must not automatically retry replacement. In particular,
sharing violations may be transient but hidden retries would add unpredictable
latency and could interact incorrectly with application-level coordination.

## Discard, drop, and abandoned files

`discard(self) -> io::Result<()>` closes and deletes the staging file without
changing the destination. It reports close or deletion errors.

Dropping an uncommitted writer performs the same cleanup on a best-effort basis
and ignores cleanup errors. Drop must not panic. Abrupt process termination can
bypass destructors and leave staging files. The default naming scheme must be
recognizable and collision-resistant so applications can implement age-gated
cleanup. The crate itself must not scan or delete unrelated files.

If commit fails, the error's staging path is diagnostic and recoverable state;
the implementation may attempt cleanup only when doing so does not obscure the
commit outcome. Callers that require positive cleanup confirmation must use
`discard` before commit or explicitly remove a path reported by a
`NotCommitted` error.

## Metadata and security descriptors

Replacement installs a new file object. By default, the crate does not promise
to preserve the destination's ACL, owner, timestamps, file ID, alternate data
streams, compression state, encryption state, extended attributes, or other
metadata. The staging file inherits applicable defaults from its parent
directory.

The Windows backend uses `MoveFileExW`, not `ReplaceFileW`. Metadata-preserving
replacement may be added only as a separate explicit option because
`ReplaceFileW` merges security and metadata, requires additional access, has
distinct partial-failure states, and does not support its nominal write-through
flag. It must not silently become the default.

## Open handles and sharing

An existing handle to the old destination remains a handle to that old file
object and may continue to read it after commit. A handle opened through the
canonical path after a successful commit observes the replacement.

Replacement may fail with `ERROR_SHARING_VIOLATION` when another handle was
opened without `FILE_SHARE_DELETE` or otherwise conflicts with the required
access. This is supported Windows behavior, not a crate defect. The crate does
not invalidate handles, request oplocks, wait for other processes, or provide
cross-process locking.

## Non-goals

The contract does not include:

- transactions spanning multiple paths;
- compare-and-swap or destination identity checks;
- cross-process locks, leases, or fencing;
- protection against malicious path or reparse-point changes;
- authentication or integrity checking of file contents;
- automatic cleanup after abrupt process termination;
- certified behavior on remote, clustered, or non-NTFS filesystems; or
- persistence when the operating system, storage stack, or hardware violates
  successful flush semantics.

## Authoritative Windows references

- [CreateFileW and sharing/write-through behavior](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew)
- [FlushFileBuffers](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)
- [MoveFileExW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)
- [Directory handles](https://learn.microsoft.com/en-us/windows/win32/fileio/obtaining-a-handle-to-a-directory)
- [ReplaceFileW metadata behavior](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)
