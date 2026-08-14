use std::io::{Cursor, Read};
use std::num::NonZeroUsize;

use super::*;
#[cfg(windows)]
use crate::windows_test::{WindowsTestEnvironment, qualify_contract_test};
use ::tokio;
use ::tokio::io::{AsyncRead, AsyncWrite};
use ::tokio::sync::oneshot;
#[cfg(windows)]
use std::path::Component;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::time::SystemTime;

const TEST_MAXIMUM: u64 = 1024;
const TEST_DOMAIN: &[u8; DOMAIN_TAG_LEN] = b"BLOBTEST";
static ARTIFACT_SEQUENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct TestDirectory {
    inner: tempfile::TempDir,
}

impl TestDirectory {
    fn path(&self) -> &std::path::Path {
        self.inner.path()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let Some(artifact_root) = std::env::var_os("ATOMIC_BLOB_TEST_ARTIFACT_DIR") else {
            return;
        };
        let test_name = std::thread::current()
            .name()
            .unwrap_or("unnamed-test")
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let sequence = ARTIFACT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let destination = std::path::PathBuf::from(artifact_root)
            .join("failed-tests")
            .join(format!("{test_name}-{}-{sequence}", std::process::id()));
        let _ = copy_test_directory(self.path(), &destination);
    }
}

fn test_directory() -> TestDirectory {
    TestDirectory {
        inner: tempfile::tempdir().unwrap(),
    }
}

fn copy_test_directory(source: &std::path::Path, destination: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_test_directory(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn format() -> BlobFormatIdentity {
    BlobFormatIdentity::new(TEST_DOMAIN, ".blob", ENVELOPE_VERSION_V1).unwrap()
}

fn options() -> AtomicBlobStoreOptions {
    AtomicBlobStoreOptions::new(format()).with_max_blob_size(TEST_MAXIMUM)
}

#[cfg(any(unix, windows))]
#[derive(Default)]
struct TestThreadExits {
    workers: std::sync::atomic::AtomicUsize,
    coordinators: std::sync::atomic::AtomicUsize,
}

#[cfg(any(unix, windows))]
impl TestThreadExits {
    fn observe(&self, stage: TestStage) {
        match stage {
            TestStage::WorkerStopped => {
                self.workers.fetch_add(1, Ordering::SeqCst);
            }
            TestStage::CoordinatorStopped => {
                self.coordinators.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    fn assert_stopped(&self, workers: usize) {
        assert_eq!(self.workers.load(Ordering::SeqCst), workers);
        assert_eq!(self.coordinators.load(Ordering::SeqCst), 1);
    }
}

#[cfg(any(unix, windows))]
fn recording_hook(
    exits: Arc<TestThreadExits>,
    hook: impl Fn(TestStage) -> std::io::Result<()> + Send + Sync + 'static,
) -> Arc<dyn Fn(TestStage) -> std::io::Result<()> + Send + Sync> {
    Arc::new(move |stage| {
        exits.observe(stage);
        hook(stage)
    })
}

#[cfg(any(unix, windows))]
async fn store_with_hook(
    root: &std::path::Path,
    namespace: &str,
    hook: Arc<dyn Fn(TestStage) -> std::io::Result<()> + Send + Sync>,
) -> AtomicBlobStore {
    AtomicBlobStore::open_with_test_hook(root, namespace, options(), hook)
        .await
        .unwrap()
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn caught_job_panic_releases_the_key_and_keeps_the_worker_usable() {
    let root = test_directory();
    let exits = Arc::new(TestThreadExits::default());
    let panic_once = Arc::new(AtomicBool::new(true));
    let hook = {
        let panic_once = Arc::clone(&panic_once);
        recording_hook(Arc::clone(&exits), move |stage| {
            if stage == TestStage::OperationStarted && panic_once.swap(false, Ordering::SeqCst) {
                panic!("test-requested operation panic");
            }
            Ok(())
        })
    };
    let store = store_with_hook(root.path(), "job-panic", hook).await;
    assert!(matches!(
        store.save(b"same-key", b"first".to_vec()).await,
        Err(AtomicBlobStoreError::EngineFailed)
    ));
    store.save(b"same-key", b"second".to_vec()).await.unwrap();
    assert_eq!(
        store.load(b"same-key").await.unwrap(),
        Some(b"second".to_vec())
    );
    assert_eq!(store.registry_entries(), 0);
    store.close().await.unwrap();
    exits.assert_stopped(1);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn worker_start_and_dispatch_failures_complete_and_release_admission() {
    for failed_stage in [TestStage::WorkerStart, TestStage::WorkerDispatch] {
        let root = test_directory();
        let exits = Arc::new(TestThreadExits::default());
        let fail_once = Arc::new(AtomicBool::new(true));
        let hook = {
            let fail_once = Arc::clone(&fail_once);
            recording_hook(Arc::clone(&exits), move |stage| {
                if stage == failed_stage && fail_once.swap(false, Ordering::SeqCst) {
                    return Err(std::io::Error::other("test-requested worker failure"));
                }
                Ok(())
            })
        };
        let store = store_with_hook(root.path(), "worker-failure", hook).await;
        assert!(matches!(
            store.save(b"same-key", b"first".to_vec()).await,
            Err(AtomicBlobStoreError::WorkerUnavailable)
        ));
        store.save(b"same-key", b"second".to_vec()).await.unwrap();
        assert_eq!(store.registry_entries(), 0);
        store.close().await.unwrap();
        exits.assert_stopped(1);
    }
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn maintenance_panic_clears_the_barrier_and_later_work_runs() {
    let root = test_directory();
    let exits = Arc::new(TestThreadExits::default());
    let panic_once = Arc::new(AtomicBool::new(true));
    let hook = {
        let panic_once = Arc::clone(&panic_once);
        recording_hook(Arc::clone(&exits), move |stage| {
            if stage == TestStage::MaintenanceStarted && panic_once.swap(false, Ordering::SeqCst) {
                panic!("test-requested maintenance panic");
            }
            Ok(())
        })
    };
    let store = store_with_hook(root.path(), "maintenance-panic", hook).await;
    assert!(matches!(
        store
            .cleanup_stale_temporary_files(Duration::from_secs(1))
            .await,
        Err(AtomicBlobStoreError::EngineFailed)
    ));
    store.save(b"later", b"value".to_vec()).await.unwrap();
    store.close().await.unwrap();
    exits.assert_stopped(1);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn maintenance_dispatch_failure_clears_the_barrier_and_later_work_runs() {
    let root = test_directory();
    let exits = Arc::new(TestThreadExits::default());
    let fail_once = Arc::new(AtomicBool::new(true));
    let hook = {
        let fail_once = Arc::clone(&fail_once);
        recording_hook(Arc::clone(&exits), move |stage| {
            if stage == TestStage::WorkerDispatch && fail_once.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other(
                    "test-requested maintenance dispatch failure",
                ));
            }
            Ok(())
        })
    };
    let store = store_with_hook(root.path(), "maintenance-dispatch", hook).await;
    assert!(matches!(
        store
            .cleanup_stale_temporary_files(Duration::from_secs(1))
            .await,
        Err(AtomicBlobStoreError::WorkerUnavailable)
    ));
    store.save(b"later", b"value".to_vec()).await.unwrap();
    assert_eq!(store.registry_entries(), 0);
    store.close().await.unwrap();
    exits.assert_stopped(1);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn coordinator_loss_is_terminal_and_deterministic() {
    let root = test_directory();
    let exits = Arc::new(TestThreadExits::default());
    let fail = Arc::new(AtomicBool::new(false));
    let hook = {
        let fail = Arc::clone(&fail);
        recording_hook(Arc::clone(&exits), move |stage| {
            if stage == TestStage::CoordinatorEvent && fail.load(Ordering::SeqCst) {
                panic!("test-requested coordinator failure");
            }
            Ok(())
        })
    };
    let store = store_with_hook(root.path(), "coordinator-loss", hook).await;
    fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        store.save(b"key", b"value".to_vec()).await,
        Err(AtomicBlobStoreError::EngineFailed)
    ));
    assert!(matches!(
        store.load(b"later").await,
        Err(AtomicBlobStoreError::EngineFailed)
    ));
    assert!(matches!(
        store.close().await,
        Err(AtomicBlobStoreError::ShutdownFailure)
    ));
    exits.assert_stopped(0);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn worker_join_panic_is_shared_by_close_callers() {
    let root = test_directory();
    let exits = Arc::new(TestThreadExits::default());
    let hook = recording_hook(Arc::clone(&exits), move |stage| {
        if stage == TestStage::WorkerExit {
            return Err(std::io::Error::other("test-requested join panic"));
        }
        Ok(())
    });
    let store = store_with_hook(root.path(), "join-panic", hook).await;
    store.save(b"key", b"value".to_vec()).await.unwrap();
    let other = store.clone();
    let (first, second) = tokio::join!(store.close(), other.close());
    assert!(matches!(first, Err(AtomicBlobStoreError::ShutdownFailure)));
    assert!(matches!(second, Err(AtomicBlobStoreError::ShutdownFailure)));
    assert!(matches!(
        store.load(b"later").await,
        Err(AtomicBlobStoreError::StoreClosed)
    ));
    exits.assert_stopped(1);
}

#[cfg(any(unix, windows))]
#[test]
fn every_concurrent_close_waits_for_the_coordinator_to_stop() {
    let root = test_directory();
    let exits = Arc::new(TestThreadExits::default());
    let stopping = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let hook = {
        let stopping = Arc::clone(&stopping);
        let release = Arc::clone(&release);
        recording_hook(Arc::clone(&exits), move |stage| {
            if stage == TestStage::CoordinatorStopping {
                stopping.wait();
                release.wait();
            }
            Ok(())
        })
    };
    let store =
        EngineHandle::open_with_test_hook(root.path(), "concurrent-join", options(), hook).unwrap();
    let first = store.close();
    let second = store.close();
    let start = Arc::new(std::sync::Barrier::new(3));
    let (finished, completion) = std::sync::mpsc::channel();

    let first_start = Arc::clone(&start);
    let first_finished = finished.clone();
    let first = std::thread::spawn(move || {
        first_start.wait();
        first_finished.send(first.wait()).unwrap();
    });
    let second_start = Arc::clone(&start);
    let second = std::thread::spawn(move || {
        second_start.wait();
        finished.send(second.wait()).unwrap();
    });

    start.wait();
    stopping.wait();
    let early_result = completion.recv_timeout(Duration::from_millis(250)).ok();
    release.wait();

    first.join().unwrap();
    second.join().unwrap();
    if let Some(result) = early_result.as_ref() {
        result.as_ref().unwrap();
    } else {
        completion.recv().unwrap().unwrap();
    }
    completion.recv().unwrap().unwrap();
    assert!(
        early_result.is_none(),
        "a close caller returned before the coordinator exited"
    );
    exits.assert_stopped(0);
}

#[cfg(any(unix, windows))]
#[test]
fn blocking_and_tokio_close_callers_share_one_engine_and_join_point() {
    let root = test_directory();
    let exits = Arc::new(TestThreadExits::default());
    let stopping = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let hook = {
        let stopping = Arc::clone(&stopping);
        let release = Arc::clone(&release);
        recording_hook(Arc::clone(&exits), move |stage| {
            if stage == TestStage::CoordinatorStopping {
                stopping.wait();
                release.wait();
            }
            Ok(())
        })
    };
    let core =
        EngineHandle::open_with_test_hook(root.path(), "mixed-close", options(), hook).unwrap();
    let blocking = BlockingAtomicBlobStore::from_test_core(core.clone());
    let asynchronous = AtomicBlobStore::from_test_core(core);
    blocking.save(b"key", b"value".to_vec()).unwrap();

    let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
    let blocking_thread = std::thread::spawn({
        let sender = finished_sender.clone();
        move || sender.send(blocking.close()).unwrap()
    });
    let async_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        finished_sender
            .send(runtime.block_on(asynchronous.close()))
            .unwrap();
    });

    stopping.wait();
    assert!(matches!(
        finished_receiver.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    release.wait();
    finished_receiver.recv().unwrap().unwrap();
    finished_receiver.recv().unwrap().unwrap();
    blocking_thread.join().unwrap();
    async_thread.join().unwrap();
    exits.assert_stopped(1);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn streaming_round_trip_is_compatible_with_complete_blob_methods() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "streaming", options())
        .await
        .unwrap();
    let payload = b"streamed payload".to_vec();
    let mut source = Cursor::new(payload.clone());
    store
        .save_from(
            b"stream-to-complete",
            &mut source,
            u64::try_from(payload.len()).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        store.load(b"stream-to-complete").await.unwrap(),
        Some(payload.clone())
    );

    store
        .save(b"complete-to-stream", payload.clone())
        .await
        .unwrap();
    let mut destination = Vec::new();
    let metadata = store
        .load_into(b"complete-to-stream", &mut destination)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(metadata.payload_len, payload.len() as u64);
    assert_eq!(destination, payload);

    let mut absent_destination = b"untouched".to_vec();
    assert_eq!(
        store
            .load_into(b"absent", &mut absent_destination)
            .await
            .unwrap(),
        None
    );
    assert_eq!(absent_destination, b"untouched");
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn streaming_transfer_uses_bounded_chunks_for_multi_chunk_payloads() {
    let root = test_directory();
    let payload_len = STREAM_CHUNK_SIZE * 3 + 17;
    let large_options = AtomicBlobStoreOptions::new(format())
        .with_max_blob_size(u64::try_from(payload_len).unwrap());
    let store = AtomicBlobStore::open(root.path(), "streaming", large_options)
        .await
        .unwrap();
    let payload = vec![0x5a; payload_len];
    let mut source = TrackingAsyncReader::new(payload.clone());
    store
        .save_from(b"large", &mut source, payload_len as u64)
        .await
        .unwrap();
    assert!(source.largest_request <= STREAM_CHUNK_SIZE);

    let mut destination = Vec::new();
    let metadata = store
        .load_into(b"large", &mut destination)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(metadata.payload_len, payload_len as u64);
    assert_eq!(destination, payload);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn streaming_save_checks_declared_length_and_preserves_old_blob() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "streaming", options())
        .await
        .unwrap();
    store.save(b"key", b"old".to_vec()).await.unwrap();

    let mut short = Cursor::new(b"new".to_vec());
    assert!(matches!(
        store.save_from(b"key", &mut short, 4).await,
        Err(AtomicBlobStoreError::InputEndedEarly {
            declared: 4,
            actual: 3
        })
    ));
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"old".to_vec()));

    let mut long = Cursor::new(b"new!".to_vec());
    assert!(matches!(
        store.save_from(b"key", &mut long, 3).await,
        Err(AtomicBlobStoreError::InputHasTrailingData { declared: 3 })
    ));
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"old".to_vec()));

    let mut over_limit = Cursor::new(Vec::<u8>::new());
    assert!(matches!(
        store
            .save_from(b"key", &mut over_limit, TEST_MAXIMUM + 1)
            .await,
        Err(AtomicBlobStoreError::BlobTooLarge {
            size,
            maximum: TEST_MAXIMUM
        }) if size == TEST_MAXIMUM + 1
    ));

    let mut failing_source = FailingAsyncReader;
    assert!(matches!(
        store.save_from(b"key", &mut failing_source, 1).await,
        Err(AtomicBlobStoreError::InputIo { .. })
    ));
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"old".to_vec()));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn streaming_save_reports_staging_failure_while_first_input_read_is_pending() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "streaming", options())
        .await
        .unwrap();
    std::fs::remove_dir(root.path().join("streaming")).unwrap();

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        store.save_from(b"key", &mut PendingAsyncReader, 1),
    )
    .await
    .expect("the completed worker error must interrupt a pending input read")
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicBlobStoreError::Io {
            operation: StoreOperation::OpenAtomicWriter,
            ..
        }
    ));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn streaming_save_reports_write_failure_while_eof_probe_is_pending() {
    let root = test_directory();
    let hook = Arc::new(|stage| {
        if stage == TestStage::DuringWrite {
            Err(io::Error::other("injected streaming write failure"))
        } else {
            Ok(())
        }
    });
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "streaming", options(), hook)
        .await
        .unwrap();
    let mut reader = PayloadThenPendingReader::new(b"new".to_vec());

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        store.save_from(b"key", &mut reader, 3),
    )
    .await
    .expect("the completed worker error must interrupt the pending EOF probe")
    .unwrap_err();
    assert!(matches!(
        error,
        AtomicBlobStoreError::Io {
            operation: StoreOperation::WriteEnvelope,
            ..
        }
    ));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn cancelling_streaming_save_during_final_eof_probe_preserves_old_blob() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "streaming", options())
        .await
        .unwrap();
    store.save(b"key", b"old".to_vec()).await.unwrap();

    let (probe_sender, probe_receiver) = oneshot::channel();
    let streaming_store = store.clone();
    let task = tokio::spawn(async move {
        let mut reader = PayloadThenPendingReader::notifying(b"new".to_vec(), probe_sender);
        streaming_store.save_from(b"key", &mut reader, 3).await
    });
    probe_receiver
        .await
        .expect("the save must enter its final EOF probe");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    store.flush().await.unwrap();
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"old".to_vec()));
    store.save(b"key", b"after".to_vec()).await.unwrap();
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"after".to_vec()));
    assert!(
        std::fs::read_dir(root.path().join("streaming"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-v1."))
    );
    store.close().await.unwrap();
}

#[cfg(all(feature = "bench-instrumentation", any(unix, windows)))]
#[tokio::test]
async fn benchmark_events_report_accepted_flush_and_actual_stream_pressure() {
    use crate::bench_instrumentation::BenchmarkEvent;

    fn receive(receiver: &std::sync::mpsc::Receiver<BenchmarkEvent>, expected: BenchmarkEvent) {
        loop {
            let event = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            if event == expected {
                return;
            }
        }
    }

    let root = test_directory();
    let (events, receiver) = std::sync::mpsc::channel();
    let receiver = Arc::new(std::sync::Mutex::new(receiver));
    let store = AtomicBlobStore::open_with_benchmark_events(
        root.path(),
        "benchmark-events",
        options().with_max_blob_size((STREAM_CHUNK_SIZE * 4) as u64),
        events,
    )
    .await
    .unwrap();

    let saving = store.clone();
    let save = tokio::spawn(async move {
        saving
            .save_from(b"source", &mut PendingAsyncReader, 1)
            .await
    });
    let source_receiver = Arc::clone(&receiver);
    tokio::task::spawn_blocking(move || {
        receive(
            &source_receiver.lock().unwrap(),
            BenchmarkEvent::SaveStreamInputStarved,
        );
    })
    .await
    .unwrap();

    let flush = store.flush();
    let flush_receiver = Arc::clone(&receiver);
    tokio::task::spawn_blocking(move || {
        receive(
            &flush_receiver.lock().unwrap(),
            BenchmarkEvent::FlushAccepted,
        );
    })
    .await
    .unwrap();
    save.abort();
    assert!(save.await.unwrap_err().is_cancelled());
    flush.await.unwrap();

    let payload = vec![7; STREAM_CHUNK_SIZE * 4];
    store.save(b"destination", payload).await.unwrap();
    let loading = store.clone();
    let load = tokio::spawn(async move {
        loading
            .load_into(b"destination", &mut BlockingAsyncWriter { started: None })
            .await
    });
    let output_receiver = Arc::clone(&receiver);
    tokio::task::spawn_blocking(move || {
        receive(
            &output_receiver.lock().unwrap(),
            BenchmarkEvent::LoadStreamOutputBackpressured,
        );
    })
    .await
    .unwrap();
    load.abort();
    assert!(load.await.unwrap_err().is_cancelled());
    store.close().await.unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn streaming_load_validates_before_writing_output() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "streaming", options())
        .await
        .unwrap();
    store.save(b"key", b"payload".to_vec()).await.unwrap();
    let path = store.blob_path(b"key");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[HEADER_LEN] ^= 1;
    std::fs::write(path, bytes).unwrap();

    let mut destination = b"unchanged".to_vec();
    assert!(matches!(
        store.load_into(b"key", &mut destination).await,
        Err(AtomicBlobStoreError::ChecksumMismatch { .. })
    ));
    assert_eq!(destination, b"unchanged");
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn streaming_destination_failure_releases_same_key_without_mutating_blob() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "streaming", options())
        .await
        .unwrap();
    store.save(b"key", b"payload".to_vec()).await.unwrap();
    let mut destination = FailingAsyncWriter;
    assert!(matches!(
        store.load_into(b"key", &mut destination).await,
        Err(AtomicBlobStoreError::OutputIo { .. })
    ));
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"payload".to_vec()));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn dropping_active_streaming_save_aborts_staging_and_releases_same_key() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "streaming", options())
        .await
        .unwrap();
    store.save(b"key", b"old".to_vec()).await.unwrap();
    let (mut source_writer, mut source_reader) = tokio::io::duplex(1);
    tokio::io::AsyncWriteExt::write_all(&mut source_writer, b"x")
        .await
        .unwrap();

    let streaming_store = store.clone();
    let task = tokio::spawn(async move {
        streaming_store
            .save_from(b"key", &mut source_reader, 2)
            .await
    });
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    store.save(b"key", b"after".to_vec()).await.unwrap();
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"after".to_vec()));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn dropping_streaming_save_after_input_completion_does_not_cancel_commit() {
    let root = test_directory();
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let hook = Arc::new(move |stage| {
        if stage == TestStage::BeforeCommit {
            started_sender.send(()).unwrap();
            release_receiver.lock().unwrap().recv().unwrap();
        }
        Ok(())
    });
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "streaming", options(), hook)
        .await
        .unwrap();
    let streaming_store = store.clone();
    let task = tokio::spawn(async move {
        streaming_store
            .save_from(b"key", &mut Cursor::new(b"new"), 3)
            .await
    });
    tokio::task::spawn_blocking(move || started_receiver.recv().unwrap())
        .await
        .unwrap();
    task.abort();
    release_sender.send(()).unwrap();
    assert!(task.await.unwrap_err().is_cancelled());

    store.flush().await.unwrap();
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"new".to_vec()));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn cancelled_streaming_load_keeps_same_key_fifo_and_not_other_keys() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "streaming", options())
        .await
        .unwrap();
    store.save(b"key", b"payload".to_vec()).await.unwrap();

    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let streaming_store = store.clone();
    let task = tokio::spawn(async move {
        let mut writer = BlockingAsyncWriter {
            started: Some(started_sender),
        };
        streaming_store.load_into(b"key", &mut writer).await
    });
    tokio::task::spawn_blocking(move || started_receiver.recv().unwrap())
        .await
        .unwrap();

    let clear = store.clear(b"key");
    let (clear_sender, mut clear_receiver) = oneshot::channel();
    tokio::spawn(async move {
        let result = clear.await;
        let _ = clear_sender.send(result);
    });
    assert!(matches!(
        clear_receiver.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    store.save(b"other", b"independent".to_vec()).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    clear_receiver.await.unwrap().unwrap();
    assert_eq!(store.load(b"key").await.unwrap(), None);
    assert_eq!(
        store.load(b"other").await.unwrap(),
        Some(b"independent".to_vec())
    );
}

#[test]
fn format_identity_rejects_invalid_domain_suffix_and_version() {
    assert!(matches!(
        BlobFormatIdentity::new(b"", ".blob", ENVELOPE_VERSION_V1),
        Err(AtomicBlobStoreConfigError::InvalidDomainTagLength { found: 0 })
    ));
    for suffix in ["", "blob", ".", ".UPPER", "../blob", ".blob.more"] {
        assert!(matches!(
            BlobFormatIdentity::new(TEST_DOMAIN, suffix, ENVELOPE_VERSION_V1),
            Err(AtomicBlobStoreConfigError::InvalidFilenameSuffix)
        ));
    }
    assert!(matches!(
        BlobFormatIdentity::new(TEST_DOMAIN, ".blob", 2),
        Err(AtomicBlobStoreConfigError::UnsupportedConfiguredEnvelopeVersion { found: 2 })
    ));
}

#[test]
fn domain_is_an_envelope_collision_guard_not_part_of_key_hashing() {
    let other = BlobFormatIdentity::new(b"BLOBOTHR", ".blob", ENVELOPE_VERSION_V1).unwrap();
    assert_eq!(
        blob_filename(&format(), b"\0\xffopaque"),
        blob_filename(&other, b"\0\xffopaque")
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn wrong_domain_fails_closed_and_flush_waits_for_submitted_work() {
    let root = test_directory();
    let first = AtomicBlobStore::open(root.path(), "shared", options())
        .await
        .unwrap();
    drop(first.save(b"\0\xffkey", b"value".to_vec()));
    first.flush().await.unwrap();

    let other = BlobFormatIdentity::new(b"BLOBOTHR", ".blob", ENVELOPE_VERSION_V1).unwrap();
    let second = AtomicBlobStore::open(
        root.path(),
        "shared",
        AtomicBlobStoreOptions::new(other).with_max_blob_size(TEST_MAXIMUM),
    )
    .await
    .unwrap();
    assert!(matches!(
        second.load(b"\0\xffkey").await,
        Err(AtomicBlobStoreError::InvalidEnvelopeDomain { .. })
    ));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn configured_concurrency_bounds_different_keys() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = test_directory();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let hook_active = Arc::clone(&active);
    let hook_maximum = Arc::clone(&maximum);
    let hook = Arc::new(move |stage| {
        if stage == TestStage::BeforeEnvelope {
            let now = hook_active.fetch_add(1, Ordering::SeqCst) + 1;
            hook_maximum.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(10));
        } else if stage == TestStage::AfterEnvelope {
            hook_active.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    });
    let bounded = options().with_max_concurrent_operations(NonZeroUsize::new(1).unwrap());
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "bounded", bounded, hook)
        .await
        .unwrap();
    let (left, right) = tokio::join!(
        store.save(b"left", b"one".to_vec()),
        store.save(b"right", b"two".to_vec())
    );
    left.unwrap();
    right.unwrap();
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
}

fn envelope(payload: &[u8]) -> Vec<u8> {
    encode_envelope(&format(), payload, TEST_MAXIMUM).unwrap()
}

#[test]
fn golden_envelope_is_stable_and_big_endian() {
    let actual = envelope(b"abc");
    assert_eq!(
        actual,
        [
            b'B', b'L', b'O', b'B', b'T', b'E', b'S', b'T', 0, 1, 0, 0, 0, 0, 0, 0, 0, 3, b'a',
            b'b', b'c', 0xb7, 0xba, 0x8c, 0x0f,
        ]
    );
}

#[test]
fn envelope_round_trip_and_empty_payload() {
    for payload in [b"payload".as_slice(), b"".as_slice()] {
        let bytes = envelope(payload);
        assert_eq!(
            decode_reader(&format(), &mut Cursor::new(bytes), TEST_MAXIMUM).unwrap(),
            payload
        );
    }
}

#[test]
fn envelope_rejects_magic_version_checksum_and_trailing_data() {
    let mut invalid_magic = envelope(b"x");
    invalid_magic[0] ^= 1;
    assert!(matches!(
        decode_reader(&format(), &mut Cursor::new(invalid_magic), TEST_MAXIMUM),
        Err(AtomicBlobStoreError::InvalidEnvelopeDomain { .. })
    ));

    let mut invalid_version = envelope(b"x");
    invalid_version[9] = 2;
    assert!(matches!(
        decode_reader(&format(), &mut Cursor::new(invalid_version), TEST_MAXIMUM),
        Err(AtomicBlobStoreError::UnsupportedEnvelopeVersion { found: 2 })
    ));

    let mut invalid_checksum = envelope(b"x");
    invalid_checksum[18] ^= 1;
    assert!(matches!(
        decode_reader(&format(), &mut Cursor::new(invalid_checksum), TEST_MAXIMUM),
        Err(AtomicBlobStoreError::ChecksumMismatch { .. })
    ));

    for suffix in [vec![1], vec![1, 2, 3]] {
        let mut trailing = envelope(b"x");
        trailing.extend_from_slice(&suffix);
        assert!(matches!(
            decode_reader(&format(), &mut Cursor::new(trailing), TEST_MAXIMUM),
            Err(AtomicBlobStoreError::TrailingData)
        ));
    }
}

#[test]
fn every_envelope_section_reports_truncation() {
    let bytes = envelope(b"abc");
    let cases = [
        (0, EnvelopeSection::Magic),
        (7, EnvelopeSection::Magic),
        (8, EnvelopeSection::Version),
        (9, EnvelopeSection::Version),
        (10, EnvelopeSection::PayloadLength),
        (17, EnvelopeSection::PayloadLength),
        (18, EnvelopeSection::Payload),
        (20, EnvelopeSection::Payload),
        (21, EnvelopeSection::Checksum),
        (24, EnvelopeSection::Checksum),
    ];
    for (length, section) in cases {
        assert!(matches!(
            decode_reader(&format(), &mut Cursor::new(&bytes[..length]), TEST_MAXIMUM),
            Err(AtomicBlobStoreError::TruncatedEnvelope { section: found }) if found == section
        ));
    }
}

#[test]
fn declared_size_is_checked_before_payload_read_or_allocation() {
    let mut bytes = Vec::from(*TEST_DOMAIN);
    bytes.extend_from_slice(&ENVELOPE_VERSION_V1.to_be_bytes());
    bytes.extend_from_slice(&(TEST_MAXIMUM + 1).to_be_bytes());
    let mut reader = CountingReader::new(bytes);
    assert!(matches!(
        decode_reader(&format(), &mut reader, TEST_MAXIMUM),
        Err(AtomicBlobStoreError::BlobTooLarge {
            size: 1025,
            maximum: TEST_MAXIMUM
        })
    ));
    assert_eq!(reader.bytes_read, HEADER_LEN);
}

#[test]
fn declared_size_above_target_usize_is_rejected_before_allocation() {
    let mut bytes = Vec::from(*TEST_DOMAIN);
    bytes.extend_from_slice(&ENVELOPE_VERSION_V1.to_be_bytes());
    bytes.extend_from_slice(&17_u64.to_be_bytes());
    let mut reader = CountingReader::new(bytes);
    assert!(matches!(
        decode_reader_with_usize_limit(&format(), &mut reader, TEST_MAXIMUM, 16),
        Err(AtomicBlobStoreError::InvalidPayloadLength { declared: 17 })
    ));
    assert_eq!(reader.bytes_read, HEADER_LEN);
}

#[test]
fn maximum_size_boundary_is_accepted_and_save_rejects_above_it() {
    let maximum = usize::try_from(TEST_MAXIMUM).unwrap();
    let payload = vec![7; maximum];
    let bytes = encode_envelope(&format(), &payload, TEST_MAXIMUM).unwrap();
    assert_eq!(
        decode_reader(&format(), &mut Cursor::new(bytes), TEST_MAXIMUM).unwrap(),
        payload
    );
    assert!(matches!(
        encode_envelope(&format(), &vec![0; maximum + 1], TEST_MAXIMUM),
        Err(AtomicBlobStoreError::BlobTooLarge { .. })
    ));
}

#[test]
fn checksum_covers_header_and_payload() {
    for index in 0..21 {
        let mut bytes = envelope(b"abc");
        if index < 8 {
            continue; // Magic has its own more precise error.
        }
        if (8..10).contains(&index) {
            continue; // Version has its own more precise error.
        }
        if (10..18).contains(&index) {
            continue; // Length mutation changes structural interpretation.
        }
        bytes[index] ^= 1;
        assert!(matches!(
            decode_reader(&format(), &mut Cursor::new(bytes), TEST_MAXIMUM),
            Err(AtomicBlobStoreError::ChecksumMismatch { .. })
        ));
    }

    let bytes = envelope(b"abc");
    assert_eq!(
        u32::from_be_bytes(bytes[21..25].try_into().unwrap()),
        crc32c::crc32c(&bytes[..21])
    );
}

#[test]
fn bounded_reader_consumes_only_declared_payload_checksum_and_one_probe() {
    let mut bytes = envelope(b"abc");
    bytes.extend(std::iter::repeat_n(9, 10_000));
    let mut reader = CountingReader::new(bytes);
    assert!(matches!(
        decode_reader(&format(), &mut reader, TEST_MAXIMUM),
        Err(AtomicBlobStoreError::TrailingData)
    ));
    assert_eq!(reader.bytes_read, HEADER_LEN + 3 + CHECKSUM_LEN + 1);
    assert_eq!(reader.largest_request, 8);
}

#[test]
fn configured_maximum_must_leave_room_for_envelope_overhead() {
    assert!(matches!(
        validate_maximum(u64::MAX),
        Err(AtomicBlobStoreError::Configuration(
            AtomicBlobStoreConfigError::InvalidMaximumBlobSize { .. }
        ))
    ));
    validate_maximum(0).unwrap();
}

struct CountingReader {
    bytes: Cursor<Vec<u8>>,
    bytes_read: usize,
    largest_request: usize,
}

impl CountingReader {
    const fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            bytes_read: 0,
            largest_request: 0,
        }
    }
}

impl Read for CountingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.largest_request = self.largest_request.max(buffer.len());
        let read = self.bytes.read(buffer)?;
        self.bytes_read += read;
        Ok(read)
    }
}

struct TrackingAsyncReader {
    bytes: Cursor<Vec<u8>>,
    largest_request: usize,
}

impl TrackingAsyncReader {
    const fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            largest_request: 0,
        }
    }
}

impl AsyncRead for TrackingAsyncReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.largest_request = self.largest_request.max(buffer.remaining());
        Pin::new(&mut self.bytes).poll_read(context, buffer)
    }
}

struct FailingAsyncReader;

impl AsyncRead for FailingAsyncReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        _buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Err(io::Error::other("injected input failure")))
    }
}

struct PendingAsyncReader;

impl AsyncRead for PendingAsyncReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        _buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Pending
    }
}

struct NotifyingPendingReader(Option<oneshot::Sender<()>>);

impl AsyncRead for NotifyingPendingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        _buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
        std::task::Poll::Pending
    }
}

struct PayloadThenPendingReader {
    bytes: Cursor<Vec<u8>>,
    pending: Option<oneshot::Sender<()>>,
}

impl PayloadThenPendingReader {
    const fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            pending: None,
        }
    }

    const fn notifying(bytes: Vec<u8>, pending: oneshot::Sender<()>) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            pending: Some(pending),
        }
    }
}

impl AsyncRead for PayloadThenPendingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        if self.bytes.position() < self.bytes.get_ref().len() as u64 {
            Pin::new(&mut self.bytes).poll_read(context, buffer)
        } else {
            if let Some(pending) = self.pending.take() {
                let _ = pending.send(());
            }
            std::task::Poll::Pending
        }
    }
}

struct FailingAsyncWriter;

impl AsyncWrite for FailingAsyncWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        _buffer: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::task::Poll::Ready(Err(io::Error::other("injected output failure")))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

struct BlockingAsyncWriter {
    started: Option<std::sync::mpsc::Sender<()>>,
}

impl AsyncWrite for BlockingAsyncWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        _buffer: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        if let Some(started) = self.started.take() {
            started.send(()).unwrap();
        }
        std::task::Poll::Pending
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[test]
fn filename_is_full_lowercase_blake3_and_contains_no_key_text() {
    let filename = blob_filename(&format(), b"../client/name\\CON:");
    assert_eq!(filename.len(), 64 + ".blob".len());
    assert_eq!(&filename[64..], ".blob");
    assert!(
        filename[..64]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    assert!(!filename.contains("client"));
    assert!(!filename.contains('/'));
    assert!(!filename.contains('\\'));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_native_save_replace_inspect_quarantine_clear_and_owned_cleanup() {
    let root = test_directory();
    let unicode_root = root.path().join("sessões-客户端");
    std::fs::create_dir(&unicode_root).unwrap();
    let store = AtomicBlobStore::open(&unicode_root, "会话-v5", options())
        .await
        .unwrap();
    let key = "ключ/客户端".as_bytes();
    store.save(key, b"old".to_vec()).await.unwrap();
    store.save(key, b"new".to_vec()).await.unwrap();
    assert_eq!(store.load(key).await.unwrap(), Some(b"new".to_vec()));
    assert_eq!(store.inspect(key).await.unwrap().state, BlobState::Present);
    let quarantine = store.quarantine(key).await.unwrap();
    assert!(quarantine.diagnostic_path.is_file());
    assert_eq!(store.load(key).await.unwrap(), None);
    store.clear(key).await.unwrap();

    let hash = "0".repeat(64);
    let owned = unicode_root
        .join("会话-v5")
        .join(format!("{hash}.blob.tmp-v1.clear.{}", "1".repeat(64)));
    let unrelated = unicode_root.join("会话-v5").join("unrelated.tmp");
    std::fs::write(&owned, b"owned").unwrap();
    std::fs::write(&unrelated, b"unrelated").unwrap();
    let report = store
        .cleanup_stale_temporary_files(Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(
        report.skipped,
        vec![owned.file_name().unwrap().to_string_lossy()]
    );
    assert!(unrelated.is_file());
    store.close().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_new_clear_staging_uses_the_clear_time_for_cleanup_age() {
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;

    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    let canonical = store.blob_path(b"old-blob");
    std::fs::write(&canonical, b"old blob").unwrap();
    let old = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
    let file = std::fs::File::options()
        .write(true)
        .open(&canonical)
        .unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();
    drop(file);

    refresh_windows_clear_age(&canonical).unwrap();
    let hash = canonical.file_stem().unwrap().to_string_lossy();
    let staging = root
        .path()
        .join("v4")
        .join(format!("{hash}.blob.tmp-v1.clear.{}", "1".repeat(64)));
    move_file(&canonical, &staging, MOVEFILE_WRITE_THROUGH).unwrap();

    let report = store
        .cleanup_stale_temporary_files(Duration::from_secs(60 * 60))
        .await
        .unwrap();
    assert!(report.removed.is_empty());
    assert_eq!(
        report.skipped,
        vec![staging.file_name().unwrap().to_string_lossy()]
    );
    assert!(staging.is_file());
}

#[cfg(windows)]
fn set_windows_modified(path: &Path, modified: SystemTime) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
}

#[cfg(windows)]
fn windows_owned_staging(namespace: &Path, kind: &str, identifier: char) -> PathBuf {
    namespace.join(format!(
        "{}.blob.tmp-v1.{kind}.{}",
        "0".repeat(64),
        identifier.to_string().repeat(64)
    ))
}

#[cfg(windows)]
fn windows_save_staging_files(namespace: &Path) -> Vec<PathBuf> {
    let mut paths = std::fs::read_dir(namespace)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            is_owned_temporary_filename(&name.to_string_lossy(), ".blob")
                && name.to_string_lossy().contains(".tmp-v1.save.")
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[cfg(windows)]
fn assert_windows_stream_failure_state(
    root: &Path,
    namespace_name: &str,
    expected_payload: &[u8],
    expected_staging: usize,
) {
    let store = BlockingAtomicBlobStore::open(root, namespace_name, options()).unwrap();
    assert_eq!(
        store.load(b"key").unwrap().as_deref(),
        Some(expected_payload)
    );
    let namespace = root.join(namespace_name);
    let staging = windows_save_staging_files(&namespace);
    assert_eq!(staging.len(), expected_staging);
    for path in &staging {
        assert_eq!(
            decode_reader(
                &format(),
                &mut Cursor::new(std::fs::read(path).unwrap()),
                TEST_MAXIMUM,
            )
            .unwrap(),
            b"new"
        );
    }
    let recent = store
        .cleanup_stale_temporary_files(Duration::from_secs(60 * 60))
        .unwrap();
    assert_eq!(recent.skipped.len(), expected_staging);
    for path in &staging {
        set_windows_modified(path, SystemTime::now() - Duration::from_secs(2 * 60 * 60));
    }
    let stale = store
        .cleanup_stale_temporary_files(Duration::from_secs(60 * 60))
        .unwrap();
    assert_eq!(stale.removed.len(), expected_staging);
    store.close().unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_complete_save_failures_have_phase_justified_states() {
    let cases = [
        (TestStage::BeforeAtomicOpen, false, false),
        (TestStage::BeforeHeaderWrite, true, false),
        (TestStage::BeforePayloadWrite, true, false),
        (TestStage::BeforeChecksumWrite, true, false),
        (TestStage::BeforeStagingFlush, true, true),
        (TestStage::BeforeCommit, true, true),
    ];
    for (failed_stage, staging_exists, staging_is_complete) in cases {
        let root = test_directory();
        if !qualify_contract_test(
            root.path(),
            &format!("deterministic-complete-save-{failed_stage:?}"),
        )
        .unwrap()
        {
            return;
        }
        let initial = AtomicBlobStore::open(root.path(), "failure-state", options())
            .await
            .unwrap();
        initial.save(b"key", b"old".to_vec()).await.unwrap();
        initial.close().await.unwrap();

        let hook = Arc::new(move |stage| {
            if stage == failed_stage {
                Err(io::Error::other("injected native Windows failure"))
            } else {
                Ok(())
            }
        });
        let store =
            AtomicBlobStore::open_with_test_hook(root.path(), "failure-state", options(), hook)
                .await
                .unwrap();
        let error = store.save(b"key", b"new".to_vec()).await.unwrap_err();
        assert!(matches!(error, AtomicBlobStoreError::Io { .. }));
        assert_eq!(store.load(b"key").await.unwrap(), Some(b"old".to_vec()));

        let staging = windows_save_staging_files(&root.path().join("failure-state"));
        assert_eq!(
            staging.len(),
            usize::from(staging_exists),
            "{failed_stage:?}"
        );
        if staging_is_complete {
            let payload = decode_reader(
                &format(),
                &mut Cursor::new(std::fs::read(&staging[0]).unwrap()),
                TEST_MAXIMUM,
            )
            .unwrap();
            assert_eq!(payload, b"new", "{failed_stage:?}");
        }
        store.close().await.unwrap();
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_move_failures_are_injected_at_the_native_call_boundary() {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_GEN_FAILURE, ERROR_SHARING_VIOLATION,
    };
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;

    #[derive(Clone, Copy, Debug)]
    enum Destination {
        Absent,
        Present,
    }

    #[derive(Clone, Copy, Debug)]
    enum Effect {
        None,
        MoveThenError,
    }

    let cases = [
        (
            "initial-definite-access-denied",
            Destination::Absent,
            Effect::None,
            ERROR_ACCESS_DENIED,
            None,
            true,
        ),
        (
            "initial-ambiguous-moved",
            Destination::Absent,
            Effect::MoveThenError,
            ERROR_GEN_FAILURE,
            Some(&b"new"[..]),
            false,
        ),
        (
            "replace-definite-sharing",
            Destination::Present,
            Effect::None,
            ERROR_SHARING_VIOLATION,
            Some(&b"old"[..]),
            true,
        ),
        (
            "replace-definite-access-denied",
            Destination::Present,
            Effect::None,
            ERROR_ACCESS_DENIED,
            Some(&b"old"[..]),
            true,
        ),
        (
            "replace-ambiguous-unchanged",
            Destination::Present,
            Effect::None,
            ERROR_GEN_FAILURE,
            Some(&b"old"[..]),
            true,
        ),
        (
            "replace-ambiguous-moved",
            Destination::Present,
            Effect::MoveThenError,
            ERROR_GEN_FAILURE,
            Some(&b"new"[..]),
            false,
        ),
    ];

    for (name, expected_destination, effect, error_code, expected, staging_expected) in cases {
        let root = test_directory();
        if !qualify_contract_test(root.path(), name).unwrap() {
            return;
        }
        if matches!(expected_destination, Destination::Present) {
            let initial = AtomicBlobStore::open(root.path(), "move-error", options())
                .await
                .unwrap();
            initial.save(b"key", b"old".to_vec()).await.unwrap();
            initial.close().await.unwrap();
        }

        let operation = Arc::new(move |source: &Path, destination: &Path, flags: u32| {
            let is_replacing = flags & MOVEFILE_REPLACE_EXISTING != 0;
            let target_call = match expected_destination {
                Destination::Absent => !is_replacing,
                Destination::Present => is_replacing,
            };
            if !target_call {
                return move_file(source, destination, flags);
            }
            if matches!(effect, Effect::MoveThenError) {
                move_file(source, destination, flags)?;
            }
            Err(io::Error::from_raw_os_error(error_code as i32))
        });
        let store = AtomicBlobStore::open_with_test_windows_move(
            root.path(),
            "move-error",
            options(),
            operation,
        )
        .await
        .unwrap();
        let error = store.save(b"key", b"new".to_vec()).await.unwrap_err();
        assert_eq!(windows_commit_raw_error(&error), Some(error_code as i32));
        drop(store);

        let fresh = AtomicBlobStore::open(root.path(), "move-error", options())
            .await
            .unwrap();
        assert_eq!(
            fresh.load(b"key").await.unwrap().as_deref(),
            expected,
            "{name}"
        );
        let namespace = root.path().join("move-error");
        let staging = windows_save_staging_files(&namespace);
        assert_eq!(staging.len(), usize::from(staging_expected), "{name}");
        if let Some(staging) = staging.first() {
            assert_eq!(
                decode_reader(
                    &format(),
                    &mut Cursor::new(std::fs::read(staging).unwrap()),
                    TEST_MAXIMUM,
                )
                .unwrap(),
                b"new",
                "{name}"
            );
            let recent = fresh
                .cleanup_stale_temporary_files(Duration::from_secs(60 * 60))
                .await
                .unwrap();
            assert_eq!(recent.skipped.len(), 1, "{name}");
            set_windows_modified(
                staging,
                SystemTime::now() - Duration::from_secs(2 * 60 * 60),
            );
            let stale = fresh
                .cleanup_stale_temporary_files(Duration::from_secs(60 * 60))
                .await
                .unwrap();
            assert_eq!(stale.removed.len(), 1, "{name}");
        }
        fresh.close().await.unwrap();
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_streaming_save_failures_preserve_old_and_clean_or_own_staging() {
    for failed_stage in [
        TestStage::BeforeHeaderWrite,
        TestStage::BeforePayloadWrite,
        TestStage::BeforeChecksumWrite,
        TestStage::BeforeStagingFlush,
        TestStage::BeforeCommit,
    ] {
        let root = test_directory();
        if !qualify_contract_test(
            root.path(),
            &format!("deterministic-streaming-save-{failed_stage:?}"),
        )
        .unwrap()
        {
            return;
        }
        let initial = AtomicBlobStore::open(root.path(), "stream-state", options())
            .await
            .unwrap();
        initial.save(b"key", b"old".to_vec()).await.unwrap();
        initial.close().await.unwrap();
        let hook = Arc::new(move |stage| {
            if stage == failed_stage {
                Err(io::Error::other("injected streaming native failure"))
            } else {
                Ok(())
            }
        });
        let store =
            AtomicBlobStore::open_with_test_hook(root.path(), "stream-state", options(), hook)
                .await
                .unwrap();
        assert!(
            store
                .save_from(b"key", &mut Cursor::new(b"new"), 3)
                .await
                .is_err()
        );
        assert_eq!(store.load(b"key").await.unwrap(), Some(b"old".to_vec()));
        for staging in windows_save_staging_files(&root.path().join("stream-state")) {
            assert!(staging.is_file());
        }
        store.close().await.unwrap();
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_streaming_input_failure_state_matrix() {
    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected blocking source failure"))
        }
    }

    for facade in ["blocking", "tokio"] {
        for failure in ["early-eof", "trailing", "source-io"] {
            let root = test_directory();
            let artifact = format!("stream-matrix-{facade}-{failure}");
            if !qualify_contract_test(root.path(), &artifact).unwrap() {
                return;
            }
            let namespace = format!("stream-{facade}-{failure}");
            let initial =
                BlockingAtomicBlobStore::open(root.path(), &namespace, options()).unwrap();
            initial.save(b"key", b"old".to_vec()).unwrap();
            initial.close().unwrap();

            if facade == "blocking" {
                let store =
                    BlockingAtomicBlobStore::open(root.path(), &namespace, options()).unwrap();
                let error = match failure {
                    "early-eof" => store
                        .save_from(b"key", &mut Cursor::new(b"new"), 4)
                        .unwrap_err(),
                    "trailing" => store
                        .save_from(b"key", &mut Cursor::new(b"new!"), 3)
                        .unwrap_err(),
                    "source-io" => store.save_from(b"key", &mut FailingReader, 1).unwrap_err(),
                    _ => unreachable!(),
                };
                assert!(match failure {
                    "early-eof" => matches!(error, AtomicBlobStoreError::InputEndedEarly { .. }),
                    "trailing" => {
                        matches!(error, AtomicBlobStoreError::InputHasTrailingData { .. })
                    }
                    "source-io" => matches!(error, AtomicBlobStoreError::InputIo { .. }),
                    _ => unreachable!(),
                });
                store.close().unwrap();
            } else {
                let store = AtomicBlobStore::open(root.path(), &namespace, options())
                    .await
                    .unwrap();
                let error = match failure {
                    "early-eof" => store
                        .save_from(b"key", &mut Cursor::new(b"new"), 4)
                        .await
                        .unwrap_err(),
                    "trailing" => store
                        .save_from(b"key", &mut Cursor::new(b"new!"), 3)
                        .await
                        .unwrap_err(),
                    "source-io" => store
                        .save_from(b"key", &mut FailingAsyncReader, 1)
                        .await
                        .unwrap_err(),
                    _ => unreachable!(),
                };
                assert!(match failure {
                    "early-eof" => matches!(error, AtomicBlobStoreError::InputEndedEarly { .. }),
                    "trailing" => {
                        matches!(error, AtomicBlobStoreError::InputHasTrailingData { .. })
                    }
                    "source-io" => matches!(error, AtomicBlobStoreError::InputIo { .. }),
                    _ => unreachable!(),
                });
                store.close().await.unwrap();
            }
            assert_windows_stream_failure_state(root.path(), &namespace, b"old", 0);
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_streaming_cancellation_and_commit_boundary_matrix() {
    {
        let root = test_directory();
        if !qualify_contract_test(root.path(), "stream-cancel-during-write").unwrap() {
            return;
        }
        let namespace = "stream-cancel-during-write";
        let initial = AtomicBlobStore::open(root.path(), namespace, options())
            .await
            .unwrap();
        initial.save(b"key", b"old".to_vec()).await.unwrap();
        initial.close().await.unwrap();
        let (reached_sender, reached_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let release_receiver = std::sync::Mutex::new(release_receiver);
        let hook = Arc::new(move |stage| {
            if stage == TestStage::DuringWrite {
                reached_sender.send(()).unwrap();
                release_receiver.lock().unwrap().recv().unwrap();
            }
            Ok(())
        });
        let store = AtomicBlobStore::open_with_test_hook(root.path(), namespace, options(), hook)
            .await
            .unwrap();
        let (mut source_writer, mut source_reader) = tokio::io::duplex(1);
        tokio::io::AsyncWriteExt::write_all(&mut source_writer, b"n")
            .await
            .unwrap();
        let streaming = store.clone();
        let task =
            tokio::spawn(async move { streaming.save_from(b"key", &mut source_reader, 2).await });
        tokio::task::spawn_blocking(move || reached_receiver.recv().unwrap())
            .await
            .unwrap();
        task.abort();
        release_sender.send(()).unwrap();
        assert!(task.await.unwrap_err().is_cancelled());
        store.flush().await.unwrap();
        store.close().await.unwrap();
        assert_windows_stream_failure_state(root.path(), namespace, b"old", 0);
    }

    {
        let root = test_directory();
        if !qualify_contract_test(root.path(), "stream-cancel-after-complete").unwrap() {
            return;
        }
        let namespace = "stream-cancel-after-complete";
        let initial = AtomicBlobStore::open(root.path(), namespace, options())
            .await
            .unwrap();
        initial.save(b"key", b"old".to_vec()).await.unwrap();
        initial.close().await.unwrap();
        let (reached_sender, reached_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let release_receiver = std::sync::Mutex::new(release_receiver);
        let hook = Arc::new(move |stage| {
            if stage == TestStage::BeforeCommit {
                reached_sender.send(()).unwrap();
                release_receiver.lock().unwrap().recv().unwrap();
            }
            Ok(())
        });
        let store = AtomicBlobStore::open_with_test_hook(root.path(), namespace, options(), hook)
            .await
            .unwrap();
        let streaming = store.clone();
        let task = tokio::spawn(async move {
            streaming
                .save_from(b"key", &mut Cursor::new(b"new"), 3)
                .await
        });
        tokio::task::spawn_blocking(move || reached_receiver.recv().unwrap())
            .await
            .unwrap();
        task.abort();
        release_sender.send(()).unwrap();
        assert!(task.await.unwrap_err().is_cancelled());
        store.flush().await.unwrap();
        store.close().await.unwrap();
        assert_windows_stream_failure_state(root.path(), namespace, b"new", 0);
    }

    {
        let root = test_directory();
        if !qualify_contract_test(root.path(), "stream-failure-before-commit").unwrap() {
            return;
        }
        let namespace = "stream-failure-before-commit";
        let initial = AtomicBlobStore::open(root.path(), namespace, options())
            .await
            .unwrap();
        initial.save(b"key", b"old".to_vec()).await.unwrap();
        initial.close().await.unwrap();
        let hook = Arc::new(|stage| {
            if stage == TestStage::BeforeCommit {
                Err(io::Error::other(
                    "injected failure immediately before commit",
                ))
            } else {
                Ok(())
            }
        });
        let store = AtomicBlobStore::open_with_test_hook(root.path(), namespace, options(), hook)
            .await
            .unwrap();
        assert!(matches!(
            store.save_from(b"key", &mut Cursor::new(b"new"), 3).await,
            Err(AtomicBlobStoreError::Io {
                operation: StoreOperation::WriteEnvelope,
                ..
            })
        ));
        store.close().await.unwrap();
        assert_windows_stream_failure_state(root.path(), namespace, b"old", 1);
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_namespace_open_and_flush_errors_are_post_commit() {
    for failed_stage in [
        TestStage::BeforeDirectoryOpen,
        TestStage::BeforeDirectoryFlush,
    ] {
        let root = test_directory();
        if !qualify_contract_test(
            root.path(),
            &format!("post-commit-namespace-sync-{failed_stage:?}"),
        )
        .unwrap()
        {
            return;
        }
        let initial = AtomicBlobStore::open(root.path(), "namespace-sync", options())
            .await
            .unwrap();
        initial.save(b"key", b"old".to_vec()).await.unwrap();
        initial.close().await.unwrap();
        let armed = Arc::new(AtomicBool::new(true));
        let hook = {
            let armed = Arc::clone(&armed);
            Arc::new(move |stage| {
                if stage == failed_stage && armed.swap(false, Ordering::SeqCst) {
                    Err(io::Error::other(
                        "injected namespace synchronization failure",
                    ))
                } else {
                    Ok(())
                }
            })
        };
        let store =
            AtomicBlobStore::open_with_test_hook(root.path(), "namespace-sync", options(), hook)
                .await
                .unwrap();
        assert!(matches!(
            store.save(b"key", b"new".to_vec()).await,
            Err(AtomicBlobStoreError::Io {
                operation: StoreOperation::SyncNamespaceDirectory,
                ..
            })
        ));
        drop(store);
        let fresh = AtomicBlobStore::open(root.path(), "namespace-sync", options())
            .await
            .unwrap();
        assert_eq!(fresh.load(b"key").await.unwrap(), Some(b"new".to_vec()));
        fresh.close().await.unwrap();
    }
}

#[cfg(windows)]
fn windows_commit_raw_error(error: &AtomicBlobStoreError) -> Option<i32> {
    match error {
        AtomicBlobStoreError::AtomicCommit { source } => source.raw_os_error(),
        _ => None,
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_sharing_violation_returns_promptly_without_retry_and_is_cleanable() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    let root = test_directory();
    if !qualify_contract_test(root.path(), "sharing-violation").unwrap() {
        return;
    }
    let store = AtomicBlobStore::open(root.path(), "sharing", options())
        .await
        .unwrap();
    store.save(b"key", b"old".to_vec()).await.unwrap();
    let canonical = store.blob_path(b"key");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&canonical)
        .unwrap();
    let started = std::time::Instant::now();
    let error = store.save(b"key", b"new".to_vec()).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        windows_commit_raw_error(&error),
        Some(ERROR_SHARING_VIOLATION as i32)
    );
    drop(held);
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"old".to_vec()));

    store.save(b"write-key", b"old".to_vec()).await.unwrap();
    let held_writer = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE)
        .open(store.blob_path(b"write-key"))
        .unwrap();
    let write_error = store.save(b"write-key", b"new".to_vec()).await.unwrap_err();
    assert_eq!(
        windows_commit_raw_error(&write_error),
        Some(ERROR_SHARING_VIOLATION as i32)
    );
    drop(held_writer);
    assert_eq!(
        store.load(b"write-key").await.unwrap(),
        Some(b"old".to_vec())
    );

    let namespace = root.path().join("sharing");
    let staging = windows_save_staging_files(&namespace);
    assert_eq!(staging.len(), 2);
    for path in &staging {
        set_windows_modified(path, SystemTime::now() - Duration::from_secs(2 * 60 * 60));
    }
    let report = store
        .cleanup_stale_temporary_files(Duration::from_secs(60 * 60))
        .await
        .unwrap();
    assert_eq!(report.removed.len(), 2);
    assert!(windows_save_staging_files(&namespace).is_empty());
    store.close().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_delete_shared_old_handle_and_fresh_open_see_distinct_file_objects() {
    use std::io::{Seek, SeekFrom};
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let root = test_directory();
    if !qualify_contract_test(root.path(), "delete-shared-old-handle").unwrap() {
        return;
    }
    let store = AtomicBlobStore::open(root.path(), "open-handle", options())
        .await
        .unwrap();
    store.save(b"key", b"old".to_vec()).await.unwrap();
    let mut old_handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(store.blob_path(b"key"))
        .unwrap();
    store.save(b"key", b"new".to_vec()).await.unwrap();

    old_handle.seek(SeekFrom::Start(0)).unwrap();
    assert_eq!(
        decode_reader(&format(), &mut old_handle, TEST_MAXIMUM).unwrap(),
        b"old"
    );
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"new".to_vec()));
    store.close().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_independent_stores_are_valid_but_have_no_ordering_contract() {
    let root = test_directory();
    if !qualify_contract_test(root.path(), "independent-store-negative-contract").unwrap() {
        return;
    }
    let left = AtomicBlobStore::open(root.path(), "independent", options())
        .await
        .unwrap();
    let right = AtomicBlobStore::open(root.path(), "independent", options())
        .await
        .unwrap();
    for iteration in 0_u8..64 {
        let left_payload = vec![b'L', iteration];
        let right_payload = vec![b'R', iteration];
        let left_save = left.save(b"key", left_payload.clone());
        let right_save = right.save(b"key", right_payload.clone());
        let (left_result, right_result) = tokio::join!(left_save, right_save);
        let observed = left.load(b"key").await.unwrap().unwrap();
        assert!(observed == left_payload || observed == right_payload);
        assert!(left_result.is_ok() || right_result.is_ok());
    }
    left.close().await.unwrap();
    right.close().await.unwrap();
}

#[cfg(windows)]
#[test]
fn windows_external_writer_is_a_bounded_concurrent_negative_contract_case() {
    let root = test_directory();
    if !qualify_contract_test(root.path(), "external-writer-negative-contract").unwrap() {
        return;
    }
    let left = BlockingAtomicBlobStore::open(root.path(), "external-writer", options()).unwrap();
    let right = BlockingAtomicBlobStore::open(root.path(), "external-writer", options()).unwrap();
    let canonical = left.blob_path(b"key");
    left.save(b"key", b"initial".to_vec()).unwrap();

    for iteration in 0_u8..64 {
        let left_payload = vec![b'L', iteration];
        let right_payload = vec![b'R', iteration];
        let external = vec![b'X'; 4 * 1024 + usize::from(iteration)];
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = left.save(b"key", left_payload.clone());
            });
            scope.spawn(|| {
                let _ = right.save(b"key", right_payload.clone());
            });
            scope.spawn(|| {
                let _ = std::fs::write(&canonical, &external);
            });

            for _ in 0..8 {
                let Ok(bytes) = std::fs::read(&canonical) else {
                    continue;
                };
                if external.starts_with(&bytes) {
                    continue;
                }
                let payload = decode_reader(&format(), &mut Cursor::new(bytes), TEST_MAXIMUM)
                    .expect("every observed store-produced candidate is a complete envelope");
                assert!(
                    payload == b"initial"
                        || matches!(payload.as_slice(), [b'L' | b'R', observed] if *observed <= iteration),
                    "store-produced envelope contained an unexpected payload"
                );
            }
        });
    }
    left.close().unwrap();
    right.close().unwrap();
}

#[cfg(windows)]
#[test]
fn windows_mapping_and_image_handle_behavior_is_characterized() {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        PAGE_READONLY, SEC_IMAGE, UnmapViewOfFile,
    };

    struct MappingView {
        _mapping: OwnedHandle,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
    }

    impl std::fmt::Debug for MappingView {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("MappingView(mapped)")
        }
    }

    impl MappingView {
        fn bytes(&self, length: usize) -> &[u8] {
            // SAFETY: the view remains mapped for `self`'s lifetime and the caller uses the
            // exact size of the backing file captured before replacement.
            unsafe { std::slice::from_raw_parts(self.view.Value.cast(), length) }
        }
    }

    impl Drop for MappingView {
        fn drop(&mut self) {
            // SAFETY: `view` is the successful result of one MapViewOfFile call and is unmapped
            // exactly once here before the mapping handle is closed.
            let _ = unsafe { UnmapViewOfFile(self.view) };
        }
    }

    fn mapping(file: &std::fs::File, protection: u32) -> io::Result<MappingView> {
        // SAFETY: the source handle remains live for the call and the unnamed mapping has no
        // security-attribute or name pointers to retain.
        let handle = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                protection,
                0,
                0,
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: the mapping handle is uniquely owned and CloseHandle-compatible.
            let mapping = unsafe { OwnedHandle::from_raw_handle(handle) };
            // SAFETY: the mapping handle is live and FILE_MAP_READ is valid for both the
            // read-only data mapping and SEC_IMAGE mapping used by this test.
            let view = unsafe { MapViewOfFile(mapping.as_raw_handle(), FILE_MAP_READ, 0, 0, 0) };
            if view.Value.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(MappingView {
                    _mapping: mapping,
                    view,
                })
            }
        }
    }

    let root = test_directory();
    if !qualify_contract_test(root.path(), "mapped-handle-characterization").unwrap() {
        return;
    }
    let store = BlockingAtomicBlobStore::open(root.path(), "mapped", options()).unwrap();
    store.save(b"key", b"old".to_vec()).unwrap();
    let canonical = store.blob_path(b"key");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&canonical)
        .unwrap();
    let mapped_length = usize::try_from(file.metadata().unwrap().len()).unwrap();
    let data_mapping = mapping(&file, PAGE_READONLY).unwrap();
    let data_result = store.save(b"key", b"new".to_vec());
    if let Err(error) = &data_result {
        assert!(matches!(
            windows_commit_raw_error(error),
            Some(code) if code == ERROR_SHARING_VIOLATION as i32 || code == ERROR_ACCESS_DENIED as i32
        ));
    }
    assert_eq!(
        decode_reader(
            &format(),
            &mut Cursor::new(data_mapping.bytes(mapped_length)),
            TEST_MAXIMUM,
        )
        .unwrap(),
        b"old"
    );
    if data_result.is_ok() {
        assert_eq!(store.load(b"key").unwrap(), Some(b"new".to_vec()));
    }
    drop(data_mapping);
    drop(file);
    store.close().unwrap();

    let image_store =
        BlockingAtomicBlobStore::open(root.path(), "mapped-image", options()).unwrap();
    let image_path = image_store.blob_path(b"key");
    std::fs::copy(std::env::current_exe().unwrap(), &image_path).unwrap();
    let image_file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&image_path)
        .unwrap();
    let image_result = mapping(&image_file, PAGE_READONLY | SEC_IMAGE);
    let image_commit_result = image_result
        .is_ok()
        .then(|| image_store.save(b"key", b"replacement".to_vec()));
    if let Some(artifact_root) = std::env::var_os("ATOMIC_BLOB_TEST_ARTIFACT_DIR") {
        let directory = PathBuf::from(artifact_root).join("handle-characterization");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("mapping.txt"),
            format!(
                "data_mapping_commit_result={data_result:?}\nimage_mapping_result={image_result:?}\nimage_mapping_commit_result={image_commit_result:?}\n"
            ),
        )
        .unwrap();
    }
    if let Ok(image_mapping) = image_result {
        if let Err(error) = image_commit_result.expect("a successful mapping attempted a commit") {
            assert!(matches!(
                windows_commit_raw_error(&error),
                Some(code) if code == ERROR_SHARING_VIOLATION as i32 || code == ERROR_ACCESS_DENIED as i32
            ));
        }
        drop(image_mapping);
    }
    drop(image_file);
    image_store.close().unwrap();
}

#[cfg(windows)]
#[test]
fn windows_directory_flush_is_characterized_on_the_actual_test_volume() {
    let root = test_directory();
    let config = initialize_platform(
        root.path().to_path_buf(),
        PathBuf::from("directory-sync"),
        format(),
        TEST_MAXIMUM,
        1,
    )
    .unwrap();
    let environment = match WindowsTestEnvironment::inspect(&config.namespace) {
        Ok(environment) => environment,
        Err(error) if std::env::var_os("ATOMIC_BLOB_REQUIRE_LOCAL_NTFS").is_none() => {
            eprintln!("directory-flush characterization unavailable: {error}");
            return;
        }
        Err(error) => panic!("failed to characterize required Windows test root: {error}"),
    };
    let result = sync_windows_directory(&config);
    if let Some(artifact_root) = std::env::var_os("ATOMIC_BLOB_TEST_ARTIFACT_DIR") {
        let directory = PathBuf::from(artifact_root).join("directory-sync");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("characterization.txt"),
            format!("{}directory_flush={result:?}\n", environment.report()),
        )
        .unwrap();
    }
    if environment.is_local_ntfs() {
        result.unwrap();
    }
}

#[cfg(windows)]
#[tokio::test]
#[allow(clippy::permissions_set_readonly_false)]
async fn windows_cleanup_classifies_owned_names_ages_and_mixed_failures() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "cleanup", options())
        .await
        .unwrap();
    let namespace = root.path().join("cleanup");
    let minimum_age = Duration::from_secs(60 * 60);
    let reference = SystemTime::now();

    let stale_save = windows_owned_staging(&namespace, "save", '1');
    let recent_save = windows_owned_staging(&namespace, "save", '2');
    let stale_clear = windows_owned_staging(&namespace, "clear", '3');
    let recent_clear = windows_owned_staging(&namespace, "clear", '4');
    let boundary = windows_owned_staging(&namespace, "save", '5');
    let removal_failure = windows_owned_staging(&namespace, "clear", '6');
    let metadata_failure = windows_owned_staging(&namespace, "save", '7');
    for path in [
        &stale_save,
        &recent_save,
        &stale_clear,
        &recent_clear,
        &boundary,
        &removal_failure,
    ] {
        std::fs::write(path, b"staging").unwrap();
    }
    std::fs::create_dir(&metadata_failure).unwrap();
    for path in [&stale_save, &stale_clear, &removal_failure] {
        set_windows_modified(path, reference - Duration::from_secs(2 * 60 * 60));
    }
    for path in [&recent_save, &recent_clear] {
        set_windows_modified(path, reference - Duration::from_secs(30 * 60));
    }
    set_windows_modified(&boundary, reference - minimum_age);
    let mut permissions = std::fs::metadata(&removal_failure).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&removal_failure, permissions).unwrap();

    let malformed = [
        format!("{}.blob.tmp-v1.save.short", "0".repeat(64)),
        format!("{}.blob.tmp-v2.save.{}", "0".repeat(64), "8".repeat(64)),
        format!("{}.wrong.tmp-v1.save.{}", "0".repeat(64), "9".repeat(64)),
        format!("{}.blob.tmp-v1.other.{}", "0".repeat(64), "a".repeat(64)),
        format!("{}.blob.tmp-v1.save.{}", "G".repeat(64), "b".repeat(64)),
        "unrelated.tmp".to_owned(),
    ];
    for name in &malformed {
        std::fs::write(namespace.join(name), b"unrelated").unwrap();
    }

    let mut report = store
        .cleanup_stale_temporary_files(minimum_age)
        .await
        .unwrap();
    report.removed.sort();
    report.skipped.sort();
    report
        .failures
        .sort_by(|left, right| left.identifier.cmp(&right.identifier));

    let mut expected_removed = [&stale_save, &stale_clear, &boundary]
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned());
    expected_removed.sort();
    let mut expected_skipped = [&recent_save, &recent_clear]
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned());
    expected_skipped.sort();
    assert_eq!(report.removed, expected_removed);
    assert_eq!(report.skipped, expected_skipped);
    assert_eq!(
        report
            .failures
            .iter()
            .map(|failure| failure.identifier.as_str())
            .collect::<Vec<_>>(),
        vec![
            removal_failure.file_name().unwrap().to_string_lossy(),
            metadata_failure.file_name().unwrap().to_string_lossy(),
        ]
    );
    for name in malformed {
        assert!(namespace.join(name).is_file());
    }

    let mut permissions = std::fs::metadata(&removal_failure).unwrap().permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(&removal_failure, permissions).unwrap();
    store.close().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_cleanup_reports_a_metadata_race_without_aborting() {
    let root = test_directory();
    let namespace = root.path().join("cleanup-race");
    std::fs::create_dir(&namespace).unwrap();
    let target = windows_owned_staging(&namespace, "save", 'c');
    std::fs::write(&target, b"staging").unwrap();
    let removed = Arc::new(AtomicBool::new(false));
    let hook = {
        let removed = Arc::clone(&removed);
        let target = target.clone();
        Arc::new(move |stage| {
            if stage == TestStage::BeforeCleanupMetadata && !removed.swap(true, Ordering::SeqCst) {
                std::fs::remove_file(&target)?;
            }
            Ok(())
        })
    };
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "cleanup-race", options(), hook)
        .await
        .unwrap();
    let report = store
        .cleanup_stale_temporary_files(Duration::from_secs(1))
        .await
        .unwrap();
    assert!(report.removed.is_empty());
    assert!(report.skipped.is_empty());
    assert_eq!(report.failures.len(), 1);
    assert_eq!(
        report.failures[0].identifier,
        target.file_name().unwrap().to_string_lossy()
    );
    assert_eq!(report.failures[0].source.kind(), io::ErrorKind::NotFound);
    store.close().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_quarantine_does_not_report_a_missing_namespace_as_a_missing_blob() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    assert!(matches!(
        store.quarantine(b"absent-key").await,
        Err(AtomicBlobStoreError::QuarantineSourceMissing)
    ));
    std::fs::remove_dir(root.path().join("v4")).unwrap();

    assert!(matches!(
        store.quarantine(b"absent-key").await,
        Err(AtomicBlobStoreError::Io {
            operation: StoreOperation::InspectNamespace,
            source,
        }) if source.kind() == io::ErrorKind::NotFound
    ));
}

#[cfg(windows)]
#[test]
fn windows_relative_root_child() {
    let Some(root) = std::env::var_os("ATOMIC_BLOB_WINDOWS_RELATIVE_ROOT_CHILD") else {
        return;
    };
    let root = PathBuf::from(root);
    std::env::set_current_dir(&root).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store =
            AtomicBlobStore::open(Path::new(".").join("unused").join(".."), "v4", options())
                .await
                .unwrap();
        let blob = store.blob_path(b"relative-key");
        assert!(blob.is_absolute());
        assert!(
            !blob
                .components()
                .any(|component| { matches!(component, Component::CurDir | Component::ParentDir) })
        );
        std::env::set_current_dir(root.join("later")).unwrap();

        store.save(b"relative-key", b"old".to_vec()).await.unwrap();
        store.save(b"relative-key", b"new".to_vec()).await.unwrap();
        assert_eq!(
            store.load(b"relative-key").await.unwrap(),
            Some(b"new".to_vec())
        );
        store.quarantine(b"relative-key").await.unwrap();
        store
            .save(b"relative-key", b"clear-me".to_vec())
            .await
            .unwrap();
        store.clear(b"relative-key").await.unwrap();
        assert_eq!(store.load(b"relative-key").await.unwrap(), None);
        store.close().await.unwrap();
    });
}

#[cfg(windows)]
#[test]
fn windows_relative_root_support_is_exercised_in_an_isolated_process() {
    let root = test_directory();
    std::fs::create_dir(root.path().join("unused")).unwrap();
    std::fs::create_dir(root.path().join("later")).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::windows_relative_root_child",
            "--nocapture",
        ])
        .env("ATOMIC_BLOB_WINDOWS_RELATIVE_ROOT_CHILD", root.path())
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(windows)]
#[test]
fn windows_extended_paths_preserve_non_unicode_wide_units() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    const BS: u16 = 92;

    let source = [
        u16::from(b'C'),
        u16::from(b':'),
        u16::from(b'/'),
        0xd800,
        u16::from(b'/'),
        u16::from(b'f'),
    ];
    let path = PathBuf::from(OsString::from_wide(&source));
    assert_eq!(
        wide_path(&path),
        [
            BS,
            BS,
            u16::from(b'?'),
            BS,
            u16::from(b'C'),
            u16::from(b':'),
            BS,
            0xd800,
            BS,
            u16::from(b'f'),
            0,
        ]
    );

    let unc = PathBuf::from(OsString::from_wide(&[BS, BS, u16::from(b's'), 0xdfff]));
    let encoded = wide_path(&unc);
    assert_eq!(
        &encoded[..8],
        &[
            BS,
            BS,
            u16::from(b'?'),
            BS,
            u16::from(b'U'),
            u16::from(b'N'),
            u16::from(b'C'),
            BS,
        ]
    );
    assert_eq!(&encoded[8..], &[u16::from(b's'), 0xdfff, 0]);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_extended_length_root_runs_real_store_operations() {
    use std::os::windows::ffi::OsStrExt;

    let temporary = test_directory();
    let mut root = temporary.path().to_path_buf();
    while root.as_os_str().encode_wide().count() <= 300 {
        root.push("long-segment-0123456789");
    }
    std::fs::create_dir_all(&root).unwrap();
    let store = AtomicBlobStore::open(&root, "namespace-长", options())
        .await
        .unwrap();
    let key = "opaque-ключ-客户端".as_bytes();
    store.save(key, b"old".to_vec()).await.unwrap();
    store.save(key, b"new".to_vec()).await.unwrap();
    assert_eq!(store.load(key).await.unwrap(), Some(b"new".to_vec()));
    let quarantine = store.quarantine(key).await.unwrap();
    assert_eq!(
        std::fs::read(quarantine.diagnostic_path).unwrap(),
        envelope(b"new")
    );
    store.save(key, b"clear".to_vec()).await.unwrap();
    store.clear(key).await.unwrap();
    assert_eq!(store.load(key).await.unwrap(), None);
    store.close().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_non_unicode_root_is_exercised_when_the_host_accepts_it() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let temporary = test_directory();
    let component = OsString::from_wide(&[
        u16::from(b'w'),
        u16::from(b'i'),
        u16::from(b'd'),
        u16::from(b'e'),
        0xd800,
    ]);
    let root = temporary.path().join(component);
    if let Err(error) = std::fs::create_dir(&root) {
        eprintln!(
            "host filesystem rejected the non-Unicode root before the crate was invoked: {error}"
        );
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::InvalidInput
                    | io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::Other
            ),
            "unexpected environmental path-creation error: {error:?}"
        );
        return;
    }

    let store = AtomicBlobStore::open(&root, "wide", options())
        .await
        .unwrap();
    store.save(b"key", b"value".to_vec()).await.unwrap();
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"value".to_vec()));
    store.clear(b"key").await.unwrap();
    store.close().await.unwrap();
}

#[cfg(windows)]
#[test]
fn windows_interruption_child() {
    use std::io::Write;

    let Some(mode) = std::env::var_os("ATOMIC_BLOB_WINDOWS_INTERRUPTION_MODE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("ATOMIC_BLOB_WINDOWS_INTERRUPTION_ROOT").unwrap());
    let stage_name = std::env::var("ATOMIC_BLOB_WINDOWS_INTERRUPTION_STAGE").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    if mode == "verify" {
        runtime.block_on(async {
            let store = AtomicBlobStore::open(&root, "interrupt", options())
                .await
                .unwrap();
            let loaded = store.load(b"key").await.unwrap();
            match stage_name.as_str() {
                "save-before-replace" | "clear-before-rename" => {
                    assert_eq!(loaded, Some(b"old".to_vec()));
                }
                "save-after-replace" => assert_eq!(loaded, Some(b"new".to_vec())),
                "clear-after-rename" | "quarantine-after-rename" => {
                    assert_eq!(loaded, None);
                }
                _ => panic!("unknown interruption stage {stage_name}"),
            }

            let namespace = root.join("interrupt");
            for entry in std::fs::read_dir(&namespace).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_owned_temporary_filename(&name, ".blob") && entry.path().is_file() {
                    let bytes = std::fs::read(entry.path()).unwrap();
                    let payload =
                        decode_reader(&format(), &mut Cursor::new(bytes), TEST_MAXIMUM).unwrap();
                    assert!(
                        payload == b"old" || payload == b"new",
                        "staging file contained an unexpected complete payload"
                    );
                }
            }
            if stage_name == "quarantine-after-rename" {
                let diagnostics = std::fs::read_dir(&namespace)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .contains(".blob.quarantine-v1.")
                    })
                    .collect::<Vec<_>>();
                assert_eq!(diagnostics.len(), 1);
                let payload = decode_reader(
                    &format(),
                    &mut Cursor::new(std::fs::read(diagnostics[0].path()).unwrap()),
                    TEST_MAXIMUM,
                )
                .unwrap();
                assert_eq!(payload, b"old");
            }
            store.close().await.unwrap();
        });
        return;
    }

    let target = match stage_name.as_str() {
        "save-before-replace" => TestStage::BeforeCommit,
        "save-after-replace" => TestStage::AfterCommit,
        "clear-before-rename" => TestStage::BeforeRemove,
        "clear-after-rename" => TestStage::AfterRemove,
        "quarantine-after-rename" => TestStage::AfterQuarantineRename,
        _ => panic!("unknown interruption stage {stage_name}"),
    };
    let signalled = AtomicBool::new(false);
    let hook = Arc::new(move |stage| {
        if stage == target && !signalled.swap(true, Ordering::SeqCst) {
            println!("ATOMIC_BLOB_STAGE_REACHED");
            std::io::stdout().flush().unwrap();
            let mut command = String::new();
            std::io::stdin().read_line(&mut command).unwrap();
        }
        Ok(())
    });
    runtime.block_on(async {
        let store = AtomicBlobStore::open_with_test_hook(&root, "interrupt", options(), hook)
            .await
            .unwrap();
        match stage_name.as_str() {
            "save-before-replace" | "save-after-replace" => {
                store.save(b"key", b"new".to_vec()).await.unwrap();
            }
            "clear-before-rename" | "clear-after-rename" => {
                store.clear(b"key").await.unwrap();
            }
            "quarantine-after-rename" => {
                store.quarantine(b"key").await.unwrap();
            }
            _ => unreachable!(),
        }
        store.close().await.unwrap();
    });
}

#[cfg(windows)]
#[test]
fn windows_child_process_interruptions_leave_only_permitted_states() {
    use std::io::{BufRead, Read};
    use std::process::{Command, Stdio};

    fn run(stage: &str) {
        let root = test_directory();
        if !qualify_contract_test(root.path(), &format!("deterministic-interruption-{stage}"))
            .unwrap()
        {
            return;
        }
        let store = BlockingAtomicBlobStore::open(root.path(), "interrupt", options()).unwrap();
        store.save(b"key", b"old".to_vec()).unwrap();
        store.close().unwrap();

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::windows_interruption_child",
                "--nocapture",
            ])
            .env("ATOMIC_BLOB_WINDOWS_INTERRUPTION_MODE", "operate")
            .env("ATOMIC_BLOB_WINDOWS_INTERRUPTION_ROOT", root.path())
            .env("ATOMIC_BLOB_WINDOWS_INTERRUPTION_STAGE", stage)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "child exited before reaching {stage}"
            );
            if line.contains("ATOMIC_BLOB_STAGE_REACHED") {
                break;
            }
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let mut child_stdout = String::new();
        output.read_to_string(&mut child_stdout).unwrap();
        let mut child_stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut child_stderr)
            .unwrap();
        if let Some(directory) = std::env::var_os("ATOMIC_BLOB_TEST_ARTIFACT_DIR") {
            let directory = PathBuf::from(directory).join(stage);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("child.stdout.log"), child_stdout).unwrap();
            std::fs::write(directory.join("child.stderr.log"), child_stderr).unwrap();
            for entry in std::fs::read_dir(root.path().join("interrupt")).unwrap() {
                let entry = entry.unwrap();
                if entry.path().is_file() {
                    std::fs::copy(entry.path(), directory.join(entry.file_name())).unwrap();
                }
            }
        }

        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::windows_interruption_child",
                "--nocapture",
            ])
            .env("ATOMIC_BLOB_WINDOWS_INTERRUPTION_MODE", "verify")
            .env("ATOMIC_BLOB_WINDOWS_INTERRUPTION_ROOT", root.path())
            .env("ATOMIC_BLOB_WINDOWS_INTERRUPTION_STAGE", stage)
            .status()
            .unwrap();
        assert!(status.success(), "verification failed for {stage}");
    }

    for stage in [
        "save-before-replace",
        "save-after-replace",
        "clear-before-rename",
        "clear-after-rename",
        "quarantine-after-rename",
    ] {
        run(stage);
    }
}

#[cfg(windows)]
fn windows_stress_payload(seed: u64, iteration: u64, size: usize) -> Vec<u8> {
    let mut payload = vec![0_u8; size];
    if size == 0 {
        return payload;
    }
    let identity = seed
        .to_le_bytes()
        .into_iter()
        .chain(iteration.to_le_bytes())
        .collect::<Vec<_>>();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = identity[index % identity.len()]
            ^ u8::try_from(index % 251).expect("the modulus fits in u8");
    }
    payload
}

#[cfg(windows)]
fn windows_stress_observation_is_valid(
    campaign: &str,
    killed: bool,
    loaded: &Result<Option<Vec<u8>>, AtomicBlobStoreError>,
    previous: Option<&[u8]>,
    valid_candidates: &[Vec<u8>],
    committed: &[u64],
) -> bool {
    killed
        && match loaded {
            Ok(None) => campaign == "create" && committed.is_empty(),
            Ok(Some(payload)) => {
                previous == Some(payload.as_slice())
                    || valid_candidates
                        .iter()
                        .any(|candidate| candidate == payload)
            }
            Err(_) => false,
        }
}

#[cfg(windows)]
#[test]
fn windows_stress_observation_rejects_absence_after_reported_create_commit() {
    let absent: Result<Option<Vec<u8>>, AtomicBlobStoreError> = Ok(None);
    assert!(windows_stress_observation_is_valid(
        "create",
        true,
        &absent,
        None,
        &[],
        &[],
    ));
    assert!(!windows_stress_observation_is_valid(
        "create",
        true,
        &absent,
        None,
        &[],
        &[7],
    ));
    assert!(!windows_stress_observation_is_valid(
        "replace",
        true,
        &absent,
        None,
        &[],
        &[],
    ));
}

#[cfg(windows)]
#[test]
fn windows_replacement_stress_child() {
    use std::io::Write;

    let Some(root) = std::env::var_os("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let seed = std::env::var("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_SEED")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let mut iteration = std::env::var("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_ITERATION")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let size = std::env::var("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_SIZE")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let streaming = std::env::var("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_API").unwrap() == "streaming";
    let tokio_facade = std::env::var("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_FACADE").unwrap() == "tokio";
    let stress_options = AtomicBlobStoreOptions::new(format()).with_max_blob_size(8 * 1024 * 1024);

    if tokio_facade {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = AtomicBlobStore::open(&root, "stress", stress_options)
                .await
                .unwrap();
            println!("READY");
            std::io::stdout().flush().unwrap();
            loop {
                let payload = windows_stress_payload(seed, iteration, size);
                println!("BEGIN {iteration}");
                std::io::stdout().flush().unwrap();
                if streaming {
                    store
                        .save_from(
                            b"key",
                            &mut Cursor::new(&payload),
                            u64::try_from(payload.len()).unwrap(),
                        )
                        .await
                        .unwrap();
                } else {
                    store.save(b"key", payload).await.unwrap();
                }
                println!("COMMITTED {iteration}");
                std::io::stdout().flush().unwrap();
                iteration = iteration.wrapping_add(1);
            }
        });
    } else {
        let store = BlockingAtomicBlobStore::open(&root, "stress", stress_options).unwrap();
        println!("READY");
        std::io::stdout().flush().unwrap();
        loop {
            let payload = windows_stress_payload(seed, iteration, size);
            println!("BEGIN {iteration}");
            std::io::stdout().flush().unwrap();
            if streaming {
                store
                    .save_from(
                        b"key",
                        &mut Cursor::new(&payload),
                        u64::try_from(payload.len()).unwrap(),
                    )
                    .unwrap();
            } else {
                store.save(b"key", payload).unwrap();
            }
            println!("COMMITTED {iteration}");
            std::io::stdout().flush().unwrap();
            iteration = iteration.wrapping_add(1);
        }
    }
}

#[cfg(windows)]
#[test]
#[ignore = "native local-NTFS evidence campaign; enabled explicitly by Windows CI"]
fn windows_randomized_replacement_stress_exposes_only_complete_candidates() {
    use std::fmt::Write as _;
    use std::io::{BufRead, Read};
    use std::process::{Command, Stdio};

    const SIZES: [usize; 5] = [
        0,
        1,
        STREAM_CHUNK_SIZE,
        3 * STREAM_CHUNK_SIZE + 17,
        4 * 1024 * 1024,
    ];

    fn next_random(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn iterations_from_output(output: &str, prefix: &str) -> Vec<u64> {
        output
            .lines()
            .filter_map(|line| line.strip_prefix(prefix))
            .filter_map(|value| value.parse().ok())
            .collect()
    }

    fn copy_failure_evidence(
        artifact_root: &Path,
        campaign: &str,
        attempt: usize,
        root: &Path,
        manifest: &str,
        stdout: &str,
        stderr: &str,
    ) {
        let destination = artifact_root
            .join("stress-failures")
            .join(format!("{campaign}-{attempt:05}"));
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("reproduction.txt"), manifest).unwrap();
        std::fs::write(destination.join("child.stdout.log"), stdout).unwrap();
        std::fs::write(destination.join("child.stderr.log"), stderr).unwrap();
        let environment = artifact_root
            .join("environments")
            .join(format!("stress-{campaign}.txt"));
        if environment.is_file() {
            std::fs::copy(environment, destination.join("environment.txt")).unwrap();
        }
        let namespace = root.join("stress");
        if namespace.is_dir() {
            for entry in std::fs::read_dir(namespace).unwrap().filter_map(Result::ok) {
                if entry.path().is_file() {
                    std::fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
                }
            }
        }
    }

    let attempts = std::env::var("ATOMIC_BLOB_WINDOWS_STRESS_ATTEMPTS")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(2_000);
    assert!(attempts >= 2);
    let create_attempts = attempts / 5;
    let replace_attempts = attempts - create_attempts;
    let seed = std::env::var("ATOMIC_BLOB_WINDOWS_STRESS_SEED")
        .ok()
        .map(|value| value.parse::<u64>().unwrap())
        .unwrap_or(0x5eed_cafe_d15c_a11e);
    let artifact_root = std::env::var_os("ATOMIC_BLOB_TEST_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("atomic-blob-store-stress-{}", std::process::id()))
        });
    std::fs::create_dir_all(&artifact_root).unwrap();
    let mut random = seed;
    let mut total_committed = 0_usize;
    let mut replacement_committed = 0_usize;
    let mut total_owned_staging = 0_usize;
    let mut summary = format!(
        "seed={seed}\nattempts={attempts}\ncreate_attempts={create_attempts}\nreplace_attempts={replace_attempts}\n"
    );

    for (campaign, campaign_attempts) in
        [("create", create_attempts), ("replace", replace_attempts)]
    {
        let root = test_directory();
        std::fs::create_dir(root.path().join("stress")).unwrap();
        assert!(qualify_contract_test(root.path(), &format!("stress-{campaign}")).unwrap());
        let stress_options =
            AtomicBlobStoreOptions::new(format()).with_max_blob_size(8 * 1024 * 1024);
        let canonical = {
            let store =
                BlockingAtomicBlobStore::open(root.path(), "stress", stress_options.clone())
                    .unwrap();
            let path = store.blob_path(b"key");
            store.close().unwrap();
            path
        };
        let mut previous = if campaign == "replace" {
            let initial = windows_stress_payload(seed, u64::MAX, 1);
            let store =
                BlockingAtomicBlobStore::open(root.path(), "stress", stress_options.clone())
                    .unwrap();
            store.save(b"key", initial.clone()).unwrap();
            store.close().unwrap();
            Some(initial)
        } else {
            None
        };

        for attempt in 0..campaign_attempts {
            if campaign == "create" {
                match std::fs::remove_file(&canonical) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => panic!("failed to reset create campaign: {error}"),
                }
                previous = None;
            }
            let global_attempt = if campaign == "create" {
                attempt
            } else {
                create_attempts + attempt
            };
            let size = SIZES[global_attempt % SIZES.len()];
            let api = if global_attempt % 3 == 0 {
                "streaming"
            } else {
                "complete"
            };
            let facade = if global_attempt % 4 == 0 {
                "tokio"
            } else {
                "blocking"
            };
            let first_iteration = u64::try_from(global_attempt).unwrap() << 32;
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::windows_replacement_stress_child",
                    "--nocapture",
                ])
                .env("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_ROOT", root.path())
                .env("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_SEED", seed.to_string())
                .env(
                    "ATOMIC_BLOB_WINDOWS_STRESS_CHILD_ITERATION",
                    first_iteration.to_string(),
                )
                .env("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_SIZE", size.to_string())
                .env("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_API", api)
                .env("ATOMIC_BLOB_WINDOWS_STRESS_CHILD_FACADE", facade)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut stdout = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(
                    output.read_line(&mut line).unwrap(),
                    0,
                    "child exited before READY"
                );
                stdout.push_str(&line);
                if line.trim() == "READY" {
                    break;
                }
            }
            let delay_us = 50 + next_random(&mut random) % 20_000;
            std::thread::sleep(Duration::from_micros(delay_us));
            let killed = child.kill().is_ok();
            let status = child.wait().unwrap();
            output.read_to_string(&mut stdout).unwrap();
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            let begun = iterations_from_output(&stdout, "BEGIN ");
            let committed = iterations_from_output(&stdout, "COMMITTED ");
            total_committed += committed.len();
            if campaign == "replace" {
                replacement_committed += committed.len();
            }
            let staging = windows_save_staging_files(&root.path().join("stress"));
            total_owned_staging += staging.len();

            let store =
                BlockingAtomicBlobStore::open(root.path(), "stress", stress_options.clone())
                    .unwrap();
            let loaded = store.load(b"key");
            store.close().unwrap();
            let valid_candidates = begun
                .iter()
                .map(|iteration| windows_stress_payload(seed, *iteration, size))
                .collect::<Vec<_>>();
            let valid = windows_stress_observation_is_valid(
                campaign,
                killed,
                &loaded,
                previous.as_deref(),
                &valid_candidates,
                &committed,
            );
            let environment_report = artifact_root
                .join("environments")
                .join(format!("stress-{campaign}.txt"));
            let mut manifest = String::new();
            writeln!(manifest, "seed={seed}").unwrap();
            writeln!(manifest, "campaign={campaign}").unwrap();
            writeln!(manifest, "attempt={attempt}").unwrap();
            writeln!(manifest, "global_attempt={global_attempt}").unwrap();
            writeln!(manifest, "delay_us={delay_us}").unwrap();
            writeln!(manifest, "size={size}").unwrap();
            writeln!(manifest, "api={api}").unwrap();
            writeln!(manifest, "facade={facade}").unwrap();
            writeln!(manifest, "first_iteration={first_iteration}").unwrap();
            writeln!(manifest, "child_status={status}").unwrap();
            writeln!(manifest, "kill_requested={killed}").unwrap();
            writeln!(manifest, "begun={begun:?}").unwrap();
            writeln!(manifest, "committed={committed:?}").unwrap();
            writeln!(manifest, "canonical={canonical:?}").unwrap();
            writeln!(manifest, "owned_staging={staging:?}").unwrap();
            writeln!(manifest, "environment_report={environment_report:?}").unwrap();
            writeln!(
                manifest,
                "reproduce=ATOMIC_BLOB_WINDOWS_STRESS_ATTEMPTS={attempts} \
                 ATOMIC_BLOB_WINDOWS_STRESS_SEED={seed} cargo test --locked --all-features \
                 tests::windows_randomized_replacement_stress_exposes_only_complete_candidates \
                 -- --ignored --exact --nocapture"
            )
            .unwrap();
            if !valid {
                copy_failure_evidence(
                    &artifact_root,
                    campaign,
                    attempt,
                    root.path(),
                    &manifest,
                    &stdout,
                    &stderr,
                );
                panic!("invalid canonical state after stress kill; {manifest}; load={loaded:?}");
            }
            if let Ok(Some(payload)) = loaded {
                previous = Some(payload);
            }
            for path in staging {
                std::fs::remove_file(path).unwrap();
            }
        }
        summary.push_str(&format!("{campaign}_completed=true\n"));
    }
    summary.push_str(&format!(
        "observed_completed_operations={total_committed}\nobserved_completed_replacements={replacement_committed}\nobserved_owned_staging_files={total_owned_staging}\n"
    ));
    std::fs::write(artifact_root.join("stress-summary.txt"), &summary).unwrap();
    assert!(
        replacement_committed > 0,
        "campaign never demonstrably completed a replacement\n{summary}"
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn save_load_replace_clear_and_missing_are_complete() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    let key = b"key";
    assert_eq!(store.load(key).await.unwrap(), None);

    store.save(key, b"old".to_vec()).await.unwrap();
    assert_eq!(store.load(key).await.unwrap(), Some(b"old".to_vec()));
    store.save(key, b"new".to_vec()).await.unwrap();
    assert_eq!(store.load(key).await.unwrap(), Some(b"new".to_vec()));
    store.clear(key).await.unwrap();
    store.clear(key).await.unwrap();
    assert_eq!(store.load(key).await.unwrap(), None);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn inspection_and_quarantine_are_non_destructive() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    let key = b"inspection-key";
    assert_eq!(store.inspect(key).await.unwrap().state, BlobState::Absent);

    store.save(key, b"canonical".to_vec()).await.unwrap();
    let inspection = store.inspect(key).await.unwrap();
    assert_eq!(inspection.state, BlobState::Present);
    assert!(inspection.size.unwrap() > b"canonical".len() as u64);
    let quarantine = store.quarantine(key).await.unwrap();
    assert_eq!(quarantine.identifier.len(), 64);
    assert!(quarantine.diagnostic_path.is_file());
    assert_eq!(store.inspect(key).await.unwrap().state, BlobState::Absent);
    assert!(matches!(
        store.quarantine(key).await,
        Err(AtomicBlobStoreError::QuarantineSourceMissing)
    ));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn quarantine_sync_failure_preserves_the_committed_destination() {
    let root = test_directory();
    let initial = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    let key = b"quarantine-sync-failure";
    initial
        .save(key, b"diagnostic payload".to_vec())
        .await
        .unwrap();
    drop(initial);

    let hook = Arc::new(|stage| {
        if stage == TestStage::BeforeDirectorySync {
            Err(io::Error::other("injected quarantine sync failure"))
        } else {
            Ok(())
        }
    });
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "v4", options(), hook)
        .await
        .unwrap();
    let error = store.quarantine(key).await.unwrap_err();
    let AtomicBlobStoreError::QuarantineNamespaceSync { quarantine, source } = error else {
        panic!("unexpected quarantine error: {error:?}");
    };
    assert_eq!(source.to_string(), "injected quarantine sync failure");
    assert!(quarantine.diagnostic_path.is_file());
    assert_eq!(
        decode_reader(
            &format(),
            &mut Cursor::new(std::fs::read(&quarantine.diagnostic_path).unwrap()),
            TEST_MAXIMUM
        )
        .unwrap(),
        b"diagnostic payload"
    );
    assert_eq!(store.inspect(key).await.unwrap().state, BlobState::Absent);
    assert_eq!(store.load(key).await.unwrap(), None);
    store.clear(key).await.unwrap();
    assert!(matches!(
        store.quarantine(key).await,
        Err(AtomicBlobStoreError::QuarantineSourceMissing)
    ));
    store.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn unix_cleanup_is_validated_and_explicitly_unsupported() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    assert!(matches!(
        store.cleanup_stale_temporary_files(Duration::ZERO).await,
        Err(AtomicBlobStoreError::InvalidCleanupAge)
    ));
    assert!(matches!(
        store
            .cleanup_stale_temporary_files(Duration::from_secs(1))
            .await,
        Err(AtomicBlobStoreError::CleanupUnsupported { .. })
    ));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn cancelled_maintenance_barrier_waits_for_earlier_work_and_blocks_later_dispatch() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = test_directory();
    let (stage_sender, stage_receiver) = std::sync::mpsc::channel();
    let stage_receiver = Arc::new(std::sync::Mutex::new(stage_receiver));
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let envelope_count = AtomicUsize::new(0);
    let hook = Arc::new(move |stage| {
        if stage == TestStage::BeforeEnvelope {
            envelope_count.fetch_add(1, Ordering::SeqCst);
            stage_sender.send(()).unwrap();
        }
        if stage == TestStage::BeforeCommit && envelope_count.load(Ordering::SeqCst) == 1 {
            release_receiver.lock().unwrap().recv().unwrap();
        }
        Ok(())
    });
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "v4", options(), hook)
        .await
        .unwrap();

    let earlier = store.save(b"earlier", b"one".to_vec());
    let first_stage_receiver = Arc::clone(&stage_receiver);
    tokio::task::spawn_blocking(move || first_stage_receiver.lock().unwrap().recv().unwrap())
        .await
        .unwrap();
    let maintenance = store.cleanup_stale_temporary_files(Duration::from_secs(1));
    drop(maintenance);
    let later = store.save(b"later", b"two".to_vec());

    // The later save cannot reach its first filesystem stage while the barrier
    // waits for the earlier save, even though the maintenance caller cancelled.
    assert!(matches!(
        stage_receiver.lock().unwrap().try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    release_sender.send(()).unwrap();
    earlier.await.unwrap();
    later.await.unwrap();
    assert_eq!(store.load(b"later").await.unwrap(), Some(b"two".to_vec()));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn maintenance_barriers_preserve_interleaved_fifo_submission_order() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = test_directory();
    let (event_sender, event_receiver) = std::sync::mpsc::channel();
    let event_receiver = Arc::new(std::sync::Mutex::new(event_receiver));
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let cleanup_count = AtomicUsize::new(0);
    let save_count = AtomicUsize::new(0);
    let hook = Arc::new(move |stage| {
        match stage {
            TestStage::BeforeCleanup => {
                let index = cleanup_count.fetch_add(1, Ordering::SeqCst);
                event_sender
                    .send(if index == 0 { "M1" } else { "M2" })
                    .unwrap();
                if index == 0 {
                    release_receiver.lock().unwrap().recv().unwrap();
                }
            }
            TestStage::BeforeEnvelope => {
                let index = save_count.fetch_add(1, Ordering::SeqCst);
                event_sender
                    .send(if index == 0 { "B" } else { "C" })
                    .unwrap();
            }
            _ => {}
        }
        Ok(())
    });
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "v4", options(), hook)
        .await
        .unwrap();

    let first_maintenance = store.cleanup_stale_temporary_files(Duration::from_secs(1));
    let first_event_receiver = Arc::clone(&event_receiver);
    assert_eq!(
        tokio::task::spawn_blocking(move || first_event_receiver.lock().unwrap().recv().unwrap())
            .await
            .unwrap(),
        "M1"
    );

    let before_second = store.save(b"before-second", b"B".to_vec());
    let second_maintenance = store.cleanup_stale_temporary_files(Duration::from_secs(1));
    let after_second = store.save(b"after-second", b"C".to_vec());
    release_sender.send(()).unwrap();

    let (first_result, before_result, second_result, after_result) = tokio::join!(
        first_maintenance,
        before_second,
        second_maintenance,
        after_second
    );
    // On Unix cleanup is unsupported; on Windows it is implemented and
    // returns an empty report when no stale files exist.
    #[cfg(unix)]
    assert!(matches!(
        first_result,
        Err(AtomicBlobStoreError::CleanupUnsupported { .. })
    ));
    #[cfg(windows)]
    first_result.unwrap();
    before_result.unwrap();
    #[cfg(unix)]
    assert!(matches!(
        second_result,
        Err(AtomicBlobStoreError::CleanupUnsupported { .. })
    ));
    #[cfg(windows)]
    second_result.unwrap();
    after_result.unwrap();

    // M2 must not overtake B, and C must not overtake M2.
    let receive_event = || event_receiver.lock().unwrap().recv().unwrap();
    assert_eq!(receive_event(), "B");
    assert_eq!(receive_event(), "M2");
    assert_eq!(receive_event(), "C");
    assert_eq!(
        store.load(b"before-second").await.unwrap(),
        Some(b"B".to_vec())
    );
    assert_eq!(
        store.load(b"after-second").await.unwrap(),
        Some(b"C".to_vec())
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn flush_barriers_are_ordered_survive_waiter_drop_and_exclude_later_work() {
    let root = test_directory();
    let (event_sender, event_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let envelope_count = std::sync::atomic::AtomicUsize::new(0);
    let commit_count = std::sync::atomic::AtomicUsize::new(0);
    let flush_count = std::sync::atomic::AtomicUsize::new(0);
    let hook = Arc::new(move |stage| {
        match stage {
            TestStage::BeforeEnvelope => {
                let index = envelope_count.fetch_add(1, Ordering::SeqCst);
                event_sender.send(format!("dispatch-{index}")).unwrap();
            }
            TestStage::BeforeCommit if commit_count.load(Ordering::SeqCst) == 0 => {
                release_receiver.lock().unwrap().recv().unwrap();
            }
            TestStage::AfterCommit => {
                let index = commit_count.fetch_add(1, Ordering::SeqCst);
                event_sender.send(format!("complete-{index}")).unwrap();
            }
            TestStage::FlushCompleted => {
                let index = flush_count.fetch_add(1, Ordering::SeqCst);
                event_sender.send(format!("flush-{index}")).unwrap();
            }
            _ => {}
        }
        Ok(())
    });
    let store = store_with_hook(root.path(), "flush-order", hook).await;

    let first = store.save(b"first", b"A".to_vec());
    assert_eq!(event_receiver.recv().unwrap(), "dispatch-0");
    let dropped_flush = store.flush();
    drop(dropped_flush);
    let second = store.save(b"second", b"B".to_vec());
    let second_flush = store.flush();
    let third = store.save(b"third", b"C".to_vec());

    assert!(matches!(
        event_receiver.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    release_sender.send(()).unwrap();
    first.await.unwrap();
    second.await.unwrap();
    second_flush.await.unwrap();
    third.await.unwrap();

    let remaining = (0..7)
        .map(|_| event_receiver.recv().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        remaining,
        [
            "complete-0",
            "flush-0",
            "dispatch-1",
            "complete-1",
            "flush-1",
            "dispatch-2",
            "complete-2",
        ]
    );
    store.flush().await.unwrap();
    store.save(b"after-flush", b"open".to_vec()).await.unwrap();
    assert_eq!(
        store.load(b"after-flush").await.unwrap(),
        Some(b"open".to_vec())
    );
    store.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn relative_root_is_stable_after_current_directory_changes() {
    struct CurrentDirectoryGuard(PathBuf);

    impl Drop for CurrentDirectoryGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).expect("restore the test process current directory");
        }
    }

    let original_directory = std::env::current_dir().unwrap();
    let _guard = CurrentDirectoryGuard(original_directory);
    let directory = test_directory();
    let initial_directory = directory.path().join("initial");
    let later_directory = directory.path().join("later");
    let root = initial_directory.join("store-root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir(&later_directory).unwrap();

    std::env::set_current_dir(&initial_directory).unwrap();
    let store = AtomicBlobStore::open("store-root", "v4", options())
        .await
        .unwrap();
    let blob = store.blob_path(b"key");
    assert!(blob.is_absolute());

    std::env::set_current_dir(&later_directory).unwrap();
    store.save(b"key", b"value".to_vec()).await.unwrap();
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"value".to_vec()));
    assert!(blob.is_file());
    assert!(!later_directory.join("store-root").exists());
}

#[cfg(any(unix, windows))]
#[test]
fn store_remains_usable_after_construction_runtime_is_dropped() {
    let root = test_directory();
    let construction_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let store = construction_runtime
        .block_on(AtomicBlobStore::open(root.path(), "v4", options()))
        .unwrap();
    let clone = store.clone();
    drop(construction_runtime);

    let save = store.save(b"key", b"value".to_vec());
    let client_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    client_runtime.block_on(save).unwrap();
    assert_eq!(
        client_runtime.block_on(clone.load(b"key")).unwrap(),
        Some(b"value".to_vec())
    );
    drop(client_runtime);

    let later_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    later_runtime.block_on(store.clear(b"key")).unwrap();
    assert_eq!(later_runtime.block_on(store.load(b"key")).unwrap(), None);
}

#[cfg(any(unix, windows))]
#[test]
fn caller_runtime_loss_before_input_completion_preserves_the_old_blob() {
    let root = test_directory();
    let construction_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let store = construction_runtime
        .block_on(AtomicBlobStore::open(
            root.path(),
            "runtime-before",
            options(),
        ))
        .unwrap();
    construction_runtime
        .block_on(store.save(b"key", b"old".to_vec()))
        .unwrap();
    drop(construction_runtime);

    let caller_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (polled_sender, polled_receiver) = oneshot::channel();
    let streaming_store = store.clone();
    caller_runtime.spawn(async move {
        let mut source = NotifyingPendingReader(Some(polled_sender));
        streaming_store.save_from(b"key", &mut source, 1).await
    });
    caller_runtime.block_on(polled_receiver).unwrap();
    drop(caller_runtime);

    let recovery_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    recovery_runtime.block_on(store.flush()).unwrap();
    assert_eq!(
        recovery_runtime.block_on(store.load(b"key")).unwrap(),
        Some(b"old".to_vec())
    );
    recovery_runtime.block_on(store.close()).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn caller_runtime_loss_after_input_completion_does_not_cancel_commit() {
    let root = test_directory();
    let armed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
    let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let hook = {
        let armed = Arc::clone(&armed);
        Arc::new(move |stage| {
            if stage == TestStage::BeforeCommit && armed.load(Ordering::SeqCst) {
                let _ = started_sender.send(());
                release_receiver.lock().unwrap().recv().unwrap();
            }
            Ok(())
        })
    };
    let construction_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let store = construction_runtime
        .block_on(AtomicBlobStore::open_with_test_hook(
            root.path(),
            "runtime-after",
            options(),
            hook,
        ))
        .unwrap();
    construction_runtime
        .block_on(store.save(b"key", b"old".to_vec()))
        .unwrap();
    armed.store(true, Ordering::SeqCst);
    drop(construction_runtime);

    let caller_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let streaming_store = store.clone();
    caller_runtime.spawn(async move {
        streaming_store
            .save_from(b"key", &mut Cursor::new(b"new"), 3)
            .await
    });
    let reached_commit = caller_runtime.spawn_blocking(move || started_receiver.recv().unwrap());
    caller_runtime.block_on(reached_commit).unwrap();
    drop(caller_runtime);
    release_sender.send(()).unwrap();

    let recovery_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    recovery_runtime.block_on(store.flush()).unwrap();
    assert_eq!(
        recovery_runtime.block_on(store.load(b"key")).unwrap(),
        Some(b"new".to_vec())
    );
    recovery_runtime.block_on(store.close()).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn accepted_complete_operation_survives_caller_runtime_loss() {
    let root = test_directory();
    let armed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
    let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let hook = {
        let armed = Arc::clone(&armed);
        Arc::new(move |stage| {
            if stage == TestStage::BeforeCommit && armed.load(Ordering::SeqCst) {
                started_sender.send(()).unwrap();
                release_receiver.lock().unwrap().recv().unwrap();
            }
            Ok(())
        })
    };
    let store =
        EngineHandle::open_with_test_hook(root.path(), "complete-runtime-loss", options(), hook)
            .map(AtomicBlobStore::from_test_core)
            .unwrap();
    armed.store(true, Ordering::SeqCst);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let saving = store.clone();
    runtime.spawn(async move { saving.save(b"key", b"value".to_vec()).await });
    let reached_commit = runtime.spawn_blocking(move || started_receiver.recv().unwrap());
    runtime.block_on(reached_commit).unwrap();
    drop(runtime);
    release_sender.send(()).unwrap();

    let recovery = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    recovery.block_on(store.flush()).unwrap();
    assert_eq!(
        recovery.block_on(store.load(b"key")).unwrap(),
        Some(b"value".to_vec())
    );
    recovery.block_on(store.close()).unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn root_and_namespace_types_are_validated() {
    let missing = test_directory().path().join("missing");
    assert!(matches!(
        AtomicBlobStore::open(missing, "v4", options()).await,
        Err(AtomicBlobStoreError::RootDoesNotExist)
    ));

    let directory = test_directory();
    let root_file = directory.path().join("file");
    std::fs::write(&root_file, b"x").unwrap();
    assert!(matches!(
        AtomicBlobStore::open(root_file, "v4", options()).await,
        Err(AtomicBlobStoreError::RootIsNotDirectory)
    ));

    std::fs::write(directory.path().join("v4"), b"x").unwrap();
    assert!(matches!(
        AtomicBlobStore::open(directory.path(), "v4", options()).await,
        Err(AtomicBlobStoreError::NamespacePathIsNotDirectory)
    ));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn missing_namespace_is_not_reported_as_a_missing_blob() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    std::fs::remove_dir(root.path().join("v4")).unwrap();
    assert!(matches!(
        store.load(b"key").await,
        Err(AtomicBlobStoreError::Io {
            operation: StoreOperation::InspectNamespace,
            ..
        })
    ));
    assert!(matches!(
        store.clear(b"key").await,
        Err(AtomicBlobStoreError::Io {
            operation: StoreOperation::InspectNamespace,
            ..
        })
    ));
}

#[test]
fn io_and_atomic_commit_errors_preserve_sources() {
    use std::error::Error;

    let io_error = AtomicBlobStoreError::Io {
        operation: StoreOperation::ReadEnvelope,
        source: io::Error::other("read"),
    };
    assert_eq!(io_error.source().unwrap().to_string(), "read");
    let commit_error = AtomicBlobStoreError::AtomicCommit {
        source: io::Error::other("commit"),
    };
    assert_eq!(commit_error.source().unwrap().to_string(), "commit");
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn cancelled_operations_remain_fifo_and_cannot_resurrect_a_blob() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    let key = b"cancelled-key";

    let save = store.save(key, vec![1; 4 * 1024 * 1024]);
    drop(save);
    store.clear(key).await.unwrap();
    assert_eq!(store.load(key).await.unwrap(), None);

    let clear = store.clear(key);
    drop(clear);
    store.save(key, b"after-clear".to_vec()).await.unwrap();
    assert_eq!(
        store.load(key).await.unwrap(),
        Some(b"after-clear".to_vec())
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn save_wrapper_failpoints_preserve_complete_old_or_new_values() {
    let before_commit = [
        TestStage::BeforeEnvelope,
        TestStage::AfterEnvelope,
        TestStage::BeforeAtomicOpen,
        TestStage::DuringWrite,
        TestStage::BeforeCommit,
        TestStage::CommitError,
    ];
    for failed_stage in before_commit {
        let root = test_directory();
        let initial = AtomicBlobStore::open(root.path(), "v4", options())
            .await
            .unwrap();
        initial.save(b"key", b"old".to_vec()).await.unwrap();
        drop(initial);

        let hook = Arc::new(move |stage| {
            if stage == failed_stage {
                Err(io::Error::other("injected save-stage failure"))
            } else {
                Ok(())
            }
        });
        let store = AtomicBlobStore::open_with_test_hook(root.path(), "v4", options(), hook)
            .await
            .unwrap();
        let error = store.save(b"key", b"new".to_vec()).await.unwrap_err();
        if failed_stage == TestStage::CommitError {
            assert!(matches!(error, AtomicBlobStoreError::AtomicCommit { .. }));
        }
        assert_eq!(store.load(b"key").await.unwrap(), Some(b"old".to_vec()));
    }

    let root = test_directory();
    let hook = Arc::new(|stage| {
        if stage == TestStage::AfterCommit {
            Err(io::Error::other("injected post-commit failure"))
        } else {
            Ok(())
        }
    });
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "v4", options(), hook)
        .await
        .unwrap();
    assert!(store.save(b"key", b"new".to_vec()).await.is_err());
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"new".to_vec()));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn streaming_save_failpoints_preserve_complete_old_or_new_values() {
    for failed_stage in [
        TestStage::BeforeEnvelope,
        TestStage::AfterEnvelope,
        TestStage::BeforeAtomicOpen,
        TestStage::DuringWrite,
        TestStage::BeforeCommit,
        TestStage::CommitError,
    ] {
        let root = test_directory();
        let initial = AtomicBlobStore::open(root.path(), "v4", options())
            .await
            .unwrap();
        initial.save(b"key", b"old".to_vec()).await.unwrap();
        drop(initial);

        let hook = Arc::new(move |stage| {
            if stage == failed_stage {
                Err(io::Error::other("injected streaming save-stage failure"))
            } else {
                Ok(())
            }
        });
        let store = AtomicBlobStore::open_with_test_hook(root.path(), "v4", options(), hook)
            .await
            .unwrap();
        let error = store
            .save_from(b"key", &mut Cursor::new(b"new"), 3)
            .await
            .unwrap_err();
        if failed_stage == TestStage::CommitError {
            assert!(matches!(error, AtomicBlobStoreError::AtomicCommit { .. }));
        }
        assert_eq!(store.load(b"key").await.unwrap(), Some(b"old".to_vec()));
    }

    let root = test_directory();
    let initial = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    initial.save(b"key", b"old".to_vec()).await.unwrap();
    initial.close().await.unwrap();
    let hook = Arc::new(|stage| {
        if stage == TestStage::AfterCommit {
            Err(io::Error::other("injected streaming post-commit failure"))
        } else {
            Ok(())
        }
    });
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "v4", options(), hook)
        .await
        .unwrap();
    assert!(
        store
            .save_from(b"key", &mut Cursor::new(b"new"), 3)
            .await
            .is_err()
    );
    assert_eq!(store.load(b"key").await.unwrap(), Some(b"new".to_vec()));
    store.close().await.unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn clear_wrapper_failpoints_expose_only_old_or_absent() {
    for failed_stage in [
        TestStage::BeforeRemove,
        TestStage::AfterRemove,
        TestStage::BeforeDirectorySync,
        TestStage::AfterDirectorySync,
    ] {
        let root = test_directory();
        let initial = AtomicBlobStore::open(root.path(), "v4", options())
            .await
            .unwrap();
        initial.save(b"key", b"old".to_vec()).await.unwrap();
        drop(initial);
        let hook = Arc::new(move |stage| {
            if stage == failed_stage {
                Err(io::Error::other("injected clear-stage failure"))
            } else {
                Ok(())
            }
        });
        let store = AtomicBlobStore::open_with_test_hook(root.path(), "v4", options(), hook)
            .await
            .unwrap();
        assert!(store.clear(b"key").await.is_err());
        let loaded = store.load(b"key").await.unwrap();
        if failed_stage == TestStage::BeforeRemove {
            assert_eq!(loaded, Some(b"old".to_vec()));
        } else {
            assert_eq!(loaded, None);
        }
    }
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn blocked_cancelled_work_keeps_same_key_order_but_not_other_keys() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let root = test_directory();
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let first = AtomicBool::new(true);
    let hook = Arc::new(move |stage| {
        if stage == TestStage::BeforeCommit && first.swap(false, Ordering::SeqCst) {
            started_sender.send(()).unwrap();
            release_receiver.lock().unwrap().recv().unwrap();
        }
        Ok(())
    });
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "v4", options(), hook)
        .await
        .unwrap();

    let cancelled = store.save(b"same", b"value".to_vec());
    drop(cancelled);
    tokio::task::spawn_blocking(move || started_receiver.recv().unwrap())
        .await
        .unwrap();

    // Submission is synchronous, so this clear is already queued behind the
    // blocked save before its future is moved into the task.
    let clear = store.clear(b"same");
    let (clear_sender, mut clear_receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        clear.await.unwrap();
        let _ = clear_sender.send(());
    });
    assert!(matches!(
        clear_receiver.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    // A different key dispatches independently while the first key is paused.
    store.save(b"other", b"concurrent".to_vec()).await.unwrap();
    assert_eq!(
        store.load(b"other").await.unwrap(),
        Some(b"concurrent".to_vec())
    );

    release_sender.send(()).unwrap();
    clear_receiver.await.unwrap();
    assert_eq!(store.load(b"same").await.unwrap(), None);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn cloned_handles_and_many_transient_keys_remain_operational() {
    // Windows synced filesystem operations are significantly slower; fewer
    // iterations keep the test within the nextest timeout budget while
    // exercising the same cloned-handle and registry-cleanup invariants.
    #[cfg(windows)]
    const TRANSIENT_KEY_CYCLES: u32 = 200;
    #[cfg(not(windows))]
    const TRANSIENT_KEY_CYCLES: u32 = 2_000;

    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    let clone = store.clone();
    for index in 0_u32..TRANSIENT_KEY_CYCLES {
        let key = index.to_be_bytes();
        let operation = clone.save(&key, key.to_vec());
        operation.await.unwrap();
        store.clear(&key).await.unwrap();
        assert_eq!(store.registry_entries(), 0);
    }
    clone.save(b"final", b"ok".to_vec()).await.unwrap();
    assert_eq!(store.load(b"final").await.unwrap(), Some(b"ok".to_vec()));
    assert_eq!(store.registry_entries(), 0);
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn close_waits_for_and_cancellation_releases_a_stalled_stream() {
    let root = test_directory();
    let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
    let hook = Arc::new(move |stage| {
        if stage == TestStage::BeforeEnvelope {
            let _ = started_sender.send(());
        }
        Ok(())
    });
    let store =
        AtomicBlobStore::open_with_test_hook(root.path(), "closing-stream", options(), hook)
            .await
            .unwrap();
    let (mut writer, mut reader) = tokio::io::duplex(1);
    tokio::io::AsyncWriteExt::write_all(&mut writer, b"x")
        .await
        .unwrap();
    let streaming_store = store.clone();
    let stream =
        tokio::spawn(async move { streaming_store.save_from(b"key", &mut reader, 2).await });
    tokio::task::spawn_blocking(move || started_receiver.recv().unwrap())
        .await
        .unwrap();

    let closing_store = store.clone();
    let close = tokio::spawn(async move { closing_store.close().await });
    while !store.is_closing() {
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        store.inspect(b"key").await,
        Err(AtomicBlobStoreError::StoreClosed)
    ));

    stream.abort();
    assert!(stream.await.unwrap_err().is_cancelled());
    close.await.unwrap().unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn close_drains_a_stream_after_input_complete_even_when_the_caller_is_cancelled() {
    let root = test_directory();
    let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
    let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let hook = Arc::new(move |stage| {
        if stage == TestStage::BeforeCommit {
            started_sender.send(()).unwrap();
            release_receiver.lock().unwrap().recv().unwrap();
        }
        Ok(())
    });
    let store =
        AtomicBlobStore::open_with_test_hook(root.path(), "close-accepted-stream", options(), hook)
            .await
            .unwrap();
    let streaming = store.clone();
    let task = tokio::spawn(async move {
        streaming
            .save_from(b"key", &mut Cursor::new(b"value"), 5)
            .await
    });
    tokio::task::spawn_blocking(move || started_receiver.recv().unwrap())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    let closing = store.clone();
    let close = tokio::spawn(async move { closing.close().await });
    tokio::task::yield_now().await;
    assert!(!close.is_finished());
    assert!(matches!(
        store.save(b"later", b"x".to_vec()).await,
        Err(AtomicBlobStoreError::StoreClosed)
    ));
    release_sender.send(()).unwrap();
    close.await.unwrap().unwrap();
    assert!(std::fs::read(store.blob_path(b"key")).is_ok());
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn last_handle_drop_drains_an_accepted_complete_operation() {
    let root = test_directory();
    let (event_sender, event_receiver) = std::sync::mpsc::channel();
    let hook = Arc::new(move |stage| {
        if matches!(
            stage,
            TestStage::AfterCommit | TestStage::WorkerStopped | TestStage::CoordinatorStopped
        ) {
            let _ = event_sender.send(stage);
        }
        Ok(())
    });
    let store = AtomicBlobStore::open_with_test_hook(root.path(), "drop-drain", options(), hook)
        .await
        .unwrap();
    let operation = store.save(b"key", b"value".to_vec());
    drop(operation);
    drop(store);

    let events = tokio::task::spawn_blocking(move || {
        (0..3)
            .map(|_| event_receiver.recv().unwrap())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert!(events.contains(&TestStage::AfterCommit));
    assert!(events.contains(&TestStage::WorkerStopped));
    assert!(events.contains(&TestStage::CoordinatorStopped));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn last_handle_drop_drains_streaming_work_after_input_completion() {
    let root = test_directory();
    let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
    let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
    let release_receiver = std::sync::Mutex::new(release_receiver);
    let (event_sender, event_receiver) = std::sync::mpsc::channel();
    let hook = Arc::new(move |stage| {
        if stage == TestStage::BeforeCommit {
            let _ = started_sender.send(());
            release_receiver.lock().unwrap().recv().unwrap();
        }
        if matches!(
            stage,
            TestStage::AfterCommit | TestStage::WorkerStopped | TestStage::CoordinatorStopped
        ) {
            let _ = event_sender.send(stage);
        }
        Ok(())
    });
    let store =
        AtomicBlobStore::open_with_test_hook(root.path(), "stream-drop-drain", options(), hook)
            .await
            .unwrap();
    let streaming_store = store.clone();
    let task = tokio::spawn(async move {
        streaming_store
            .save_from(b"key", &mut Cursor::new(b"value"), 5)
            .await
    });
    tokio::task::spawn_blocking(move || started_receiver.recv().unwrap())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(store);
    release_sender.send(()).unwrap();

    let events = tokio::task::spawn_blocking(move || {
        (0..3)
            .map(|_| event_receiver.recv().unwrap())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert!(events.contains(&TestStage::AfterCommit));
    assert!(events.contains(&TestStage::WorkerStopped));
    assert!(events.contains(&TestStage::CoordinatorStopped));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn last_handle_drop_aborts_a_pre_complete_stream_and_drains_bookkeeping() {
    let root = test_directory();
    let exits = Arc::new(TestThreadExits::default());
    let armed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
    let (stopped_sender, stopped_receiver) = std::sync::mpsc::sync_channel(1);
    let hook = {
        let armed = Arc::clone(&armed);
        recording_hook(Arc::clone(&exits), move |stage| {
            if stage == TestStage::BeforeEnvelope && armed.load(Ordering::SeqCst) {
                started_sender.send(()).unwrap();
            }
            if stage == TestStage::CoordinatorStopped {
                let _ = stopped_sender.send(());
            }
            Ok(())
        })
    };
    let core = EngineHandle::open_with_test_hook(root.path(), "drop-pre-complete", options(), hook)
        .unwrap();
    let registry = Arc::clone(&core.inner.registry_entries);
    let store = AtomicBlobStore::from_test_core(core.clone());
    store.save(b"key", b"old".to_vec()).await.unwrap();
    armed.store(true, Ordering::SeqCst);

    let streaming = store.clone();
    let task = tokio::spawn(async move {
        let mut source = NotifyingPendingReader(None);
        streaming.save_from(b"key", &mut source, 1).await
    });
    tokio::task::spawn_blocking(move || started_receiver.recv().unwrap())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(store);
    drop(core);

    tokio::task::spawn_blocking(move || stopped_receiver.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .unwrap();
    exits.assert_stopped(1);
    assert_eq!(registry.load(Ordering::SeqCst), 0);

    let reopened = AtomicBlobStore::open(root.path(), "drop-pre-complete", options())
        .await
        .unwrap();
    assert_eq!(reopened.load(b"key").await.unwrap(), Some(b"old".to_vec()));
    reopened.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn save_uses_owner_only_mode_and_does_not_preserve_broader_mode() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    let key = b"mode-key";
    store.save(key, b"one".to_vec()).await.unwrap();
    let path = store.blob_path(key);
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        let path_bytes = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path_bytes` is a valid NUL-terminated path for this call.
        assert_eq!(unsafe { libc::chown(path_bytes.as_ptr(), 1, 1) }, 0);
    }
    store.save(key, b"two".to_vec()).await.unwrap();
    let metadata = std::fs::metadata(path).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o600);
    // SAFETY: `geteuid` has no preconditions.
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
}

#[cfg(unix)]
#[test]
fn actual_atomic_writer_preserves_old_value_until_commit_and_drop_discards() {
    use atomic_write_file::unix::OpenOptionsExt as AtomicOpenOptionsExt;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt as StdOpenOptionsExt;

    let root = test_directory();
    let path = root.path().join("blob");
    std::fs::write(&path, b"old").unwrap();

    let open = || {
        let mut options = atomic_write_file::OpenOptions::new();
        StdOpenOptionsExt::mode(&mut options, 0o600);
        AtomicOpenOptionsExt::preserve_mode(&mut options, false);
        AtomicOpenOptionsExt::preserve_owner(&mut options, false);
        options.open(&path).unwrap()
    };

    let mut writer = open();
    writer.write_all(b"new").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    drop(writer);
    assert_eq!(std::fs::read(&path).unwrap(), b"old");

    let mut writer = open();
    writer.write_all(b"new").unwrap();
    writer.commit().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"new");
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn canonical_load_ignores_unrelated_temporary_files() {
    let root = test_directory();
    let store = AtomicBlobStore::open(root.path(), "v4", options())
        .await
        .unwrap();
    let key = b"key";
    std::fs::write(
        root.path().join("v4/.unrelated.temporary"),
        envelope(b"fake"),
    )
    .unwrap();
    assert_eq!(store.load(key).await.unwrap(), None);
}

#[cfg(unix)]
#[test]
fn atomic_child_boundary() {
    use atomic_write_file::unix::OpenOptionsExt as AtomicOpenOptionsExt;
    use std::io::{BufRead, Write};
    use std::os::unix::fs::OpenOptionsExt as StdOpenOptionsExt;

    let Ok(path) = std::env::var("ATOMIC_BLOB_CHILD_PATH") else {
        return;
    };
    let payload = std::env::var("ATOMIC_BLOB_CHILD_PAYLOAD").unwrap();
    let mut options = atomic_write_file::OpenOptions::new();
    StdOpenOptionsExt::mode(&mut options, 0o600);
    AtomicOpenOptionsExt::preserve_mode(&mut options, false);
    AtomicOpenOptionsExt::preserve_owner(&mut options, false);
    let mut writer = options.open(path).unwrap();
    writer.write_all(&envelope(payload.as_bytes())).unwrap();
    println!("READY");

    let mut command = String::new();
    std::io::stdin().lock().read_line(&mut command).unwrap();
    if command.trim() == "commit" {
        writer.commit().unwrap();
        println!("COMMITTED");
    }
}

#[cfg(unix)]
#[test]
fn subprocess_exit_before_commit_and_successful_commit_have_permitted_states() {
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};

    fn spawn_child(path: &Path, payload: &str) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::atomic_child_boundary", "--nocapture"])
            .env("ATOMIC_BLOB_CHILD_PATH", path)
            .env("ATOMIC_BLOB_CHILD_PAYLOAD", payload)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn wait_for(reader: &mut impl BufRead, expected: &str) {
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line.contains(expected) {
                return;
            }
        }
    }

    let root = test_directory();
    let path = root.path().join("blob.blob");
    std::fs::write(&path, envelope(b"old")).unwrap();

    let mut interrupted = spawn_child(&path, "new");
    let mut output = std::io::BufReader::new(interrupted.stdout.take().unwrap());
    wait_for(&mut output, "READY");
    interrupted.kill().unwrap();
    interrupted.wait().unwrap();
    assert_eq!(
        decode_reader(
            &format(),
            &mut Cursor::new(std::fs::read(&path).unwrap()),
            TEST_MAXIMUM
        )
        .unwrap(),
        b"old"
    );

    let mut committed = spawn_child(&path, "new");
    let mut output = std::io::BufReader::new(committed.stdout.take().unwrap());
    wait_for(&mut output, "READY");
    committed
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"commit\n")
        .unwrap();
    wait_for(&mut output, "COMMITTED");
    assert!(committed.wait().unwrap().success());
    assert_eq!(
        decode_reader(
            &format(),
            &mut Cursor::new(std::fs::read(path).unwrap()),
            TEST_MAXIMUM
        )
        .unwrap(),
        b"new"
    );
}
