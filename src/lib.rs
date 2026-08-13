//! Interruption-resistant keyed blob streaming on trusted local filesystems.
//!
//! The store accepts opaque key and payload bytes. [`BlockingAtomicBlobStore`]
//! and the feature-gated async `tokio::AtomicBlobStore` facade use one
//! executor-neutral engine with bounded streaming; complete load/save methods
//! remain allocation conveniences. See the crate README for the exact format,
//! lifecycle, cancellation behavior, and trust boundary.
//!
//! # Platform and filesystem scope
//!
//! This implementation runs on Unix and Windows. Other targets compile, but
//! opening a store returns [`AtomicBlobStoreError::UnsupportedPlatform`]. Its
//! filesystem guarantees are limited to the native filesystems and test
//! environments described below; successful operation on another filesystem
//! does not certify that filesystem's interruption or durability behavior.
//!
//! A successful save means the platform backend synchronized its staging file
//! and completed its same-directory replacement operation. Unix uses
//! `atomic-write-file`. Windows uses an exclusively created staging file,
//! `FlushFileBuffers`, and `MoveFileExW` with write-through flags, followed by a
//! namespace-directory flush that must succeed for the operation to report
//! success.
//!
//! On native local filesystems with rename semantics matching the tested
//! environments, this design is intended to provide complete old-or-new
//! canonical-path visibility under process interruption. Windows interruption
//! evidence currently covers local NTFS and termination immediately before and
//! after the replacement call; atomicity while interruption overlaps the Win32
//! call is an engineering expectation, not an explicit universal Win32 or
//! cross-filesystem guarantee. ReFS, FAT/exFAT, SMB, cloud-backed directories,
//! container or virtual volumes, and other filesystems are not certified.
//!
//! Flush and write-through calls request persistence from the operating system
//! and storage stack. They cannot guarantee survival under arbitrary power
//! loss, controller or device caches that ignore flush requests, filesystem
//! defects, or virtual-storage behavior. Windows does not document the
//! directory flush used here as a portable equivalent of Unix directory
//! `fsync`; it is an additional checked operation, not the basis of the
//! old-or-new visibility expectation.
//!
//! # Trust and concurrency model
//!
//! The configured root and its ancestors must be trusted and controlled by the
//! application. Hash-derived filenames prevent canonical blob key bytes from
//! directly constructing paths outside that root. The store does not defend
//! against another process or attacker that can modify the root, symlink or
//! directory replacement, Windows reparse points, or concurrent blob
//! manipulation.
//!
//! Coordination is process-local and shared by clones of one store. It provides
//! same-key FIFO execution and cancellation-safe completion, but no
//! cross-process locking, distributed locking, leases, fencing,
//! compare-and-swap, or multi-writer coordination. Applications must ensure
//! only one process and one active blob owner writes a key.
//!
//! # Data and recovery limitations
//!
//! CRC32C detects accidental corruption only. The store provides no encryption,
//! authentication, cryptographic integrity, or tamper resistance. Corruption
//! fails closed and is left untouched.
//!
//! Errors do not always imply that the namespace is unchanged. An atomic commit
//! error is ambiguous and requires a reload. A save can also fail while
//! synchronizing the namespace after the complete replacement is already
//! canonical; clear can fail after its rename has already made the canonical
//! path absent. Callers that need the resulting state must inspect or reload.
//!
//! Only the canonical configured-suffix path is authoritative. Windows cleanup
//! recognizes only store-owned staging names; Unix never parses
//! dependency-private temporary names.

mod blocking;
pub use blocking::BlockingAtomicBlobStore;

#[cfg(feature = "tokio")]
pub mod tokio;

#[cfg(any(unix, windows))]
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
#[cfg(any(unix, windows))]
use std::io;
#[cfg(any(unix, windows))]
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(any(unix, windows))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::{Receiver, Sender};

mod config;
#[cfg(any(unix, windows))]
use config::STREAM_CHANNEL_CAPACITY;
pub use config::{
    AtomicBlobStoreConfigError, AtomicBlobStoreOptions, BlobFormatIdentity, DEFAULT_MAX_BLOB_SIZE,
    DEFAULT_MAX_CONCURRENT_OPERATIONS, DOMAIN_TAG_LEN, ENVELOPE_VERSION_V1,
    MAX_FILENAME_SUFFIX_LEN,
};
use config::{CHECKSUM_LEN, HEADER_LEN, STREAM_CHUNK_SIZE};
mod error;
pub use error::{
    AtomicBlobStoreError, BlobInspection, BlobMetadata, BlobState, CleanupFailure, CleanupReport,
    EnvelopeSection, QuarantineInfo, StoreOperation,
};
mod engine;
#[cfg(all(test, feature = "tokio", any(unix, windows)))]
use engine::Lifecycle;
#[cfg(feature = "tokio")]
use engine::Pending;
#[cfg(any(unix, windows))]
use engine::StoreConfig;
#[cfg(all(feature = "bench-instrumentation", any(unix, windows)))]
use engine::emit_benchmark_event;
use engine::{EngineHandle, LoadStreamEndpoint, SaveStreamEndpoint, SaveStreamMessage};
#[cfg(all(test, feature = "tokio", any(unix, windows)))]
use engine::{TestStage, validate_maximum};
#[cfg(any(unix, windows))]
mod format;
#[cfg(all(
    any(unix, windows),
    any(feature = "bench-instrumentation", all(test, feature = "tokio"))
))]
use format::decode_reader;
#[cfg(all(test, feature = "tokio", any(unix, windows)))]
use format::decode_reader_with_usize_limit;
#[cfg(all(
    any(unix, windows),
    any(feature = "bench-instrumentation", all(test, feature = "tokio"))
))]
use format::encode_envelope;
#[cfg(any(unix, windows))]
mod filesystem;
#[cfg(any(unix, windows))]
use filesystem::{
    cleanup_stale_files, clear_blob, ensure_namespace_available, initialize_platform, inspect_blob,
    quarantine_blob, save_blob, save_blob_from_receiver,
};
#[cfg(all(test, feature = "tokio", windows))]
use filesystem::{is_owned_temporary_filename, move_file, refresh_windows_clear_age, wide_path};
#[cfg(any(unix, windows))]
#[allow(unused_imports)]
use format::{
    envelope_header, envelope_parts, load_blob, load_blob_into_sender, write_stream_envelope,
};
mod path;
pub use path::blob_filename;
#[cfg(any(unix, windows))]
use path::key_hash;
use path::validate_namespace;

/// Benchmark-only access to the stable production envelope implementation.
/// This module is deliberately feature-gated so ordinary consumers do not
/// acquire an additional public surface. Its functions call the same encoder
/// and bounded reader used by the store facades.
#[cfg(all(feature = "bench-instrumentation", any(unix, windows)))]
#[doc(hidden)]
pub mod bench_instrumentation {
    use std::io::Read;

    use super::{AtomicBlobStoreError, CHECKSUM_LEN, HEADER_LEN, decode_reader, encode_envelope};

    pub const ENVELOPE_OVERHEAD: usize = HEADER_LEN + CHECKSUM_LEN;
    pub const STREAM_CHUNK_BYTES: usize = super::STREAM_CHUNK_SIZE;
    pub const STREAM_CHANNEL_CAPACITY: usize = super::STREAM_CHANNEL_CAPACITY;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum BenchmarkEvent {
        FlushAccepted,
        SaveStreamInputStarved,
        LoadStreamOutputBackpressured,
    }

    pub fn encode(
        format: &super::BlobFormatIdentity,
        payload: &[u8],
        maximum: u64,
    ) -> Result<Vec<u8>, AtomicBlobStoreError> {
        encode_envelope(format, payload, maximum)
    }

    pub fn decode(
        format: &super::BlobFormatIdentity,
        reader: &mut impl Read,
        maximum: u64,
    ) -> Result<Vec<u8>, AtomicBlobStoreError> {
        decode_reader(format, reader, maximum)
    }
}

#[cfg(all(test, feature = "tokio", any(unix, windows)))]
use tokio::AtomicBlobStore;

#[cfg(all(test, feature = "tokio", any(unix, windows)))]
mod tests;
