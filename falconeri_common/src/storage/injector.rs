//! Testing: a bare-bones fault-injecting [`ObjectStore`].
//!
//! [`InMemory`] never fails, which makes the error paths we actually care
//! about untestable against it: whether an upload failure reaches
//! `abort()`, and whether small files take the `put` path or the
//! multipart path. [`ObjectStoreFaultInjector`] wraps `InMemory`, records
//! every call that matters, and can fail the Nth call of a kind.
//!
//! This is deliberately the smallest thing that works, per
//! `plans/CLOUD_STORAGE_IO.md` §2.4: it records and fails only the calls
//! the beta tests need, and delegates everything else silently. When a
//! later test needs a new call kind or a specific error variant, extend
//! it then rather than generalizing now.

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    Result as OsResult, UploadPart, memory::InMemory, path::Path,
};

/// A store call the injector distinguishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallKind {
    /// `put_opts`—what the convenience method `put` calls.
    Put,
    /// `put_multipart_opts`—what `put_multipart` calls.
    PutMultipart,
    /// `MultipartUpload::put_part` on an upload we handed out.
    PutPart,
    /// `MultipartUpload::complete` on an upload we handed out.
    Complete,
    /// `MultipartUpload::abort` on an upload we handed out. Note that
    /// `abort` is *not* a store method: reaching it must be observed on
    /// the upload object, which is why this injector wraps those too.
    Abort,
    /// `get_opts`, which is also where `get`, `get_range`, and `head`
    /// arrive. Not distinguished further (yet).
    Get,
}

/// Call log plus armed faults, shared between the injector and the upload
/// wrappers it hands out.
#[derive(Debug, Default)]
struct State {
    /// Every recorded call, in order.
    log: Vec<CallKind>,
    /// Armed faults: fail the Nth (1-based) call of a kind. Each fires at
    /// most once.
    faults: Vec<(CallKind, usize)>,
}

impl State {
    /// Record one call, failing it if a fault is armed for its ordinal.
    fn record(&mut self, kind: CallKind) -> OsResult<()> {
        self.log.push(kind);
        let nth = self.log.iter().filter(|k| **k == kind).count();
        if let Some(i) = self
            .faults
            .iter()
            .position(|(k, n)| *k == kind && *n == nth)
        {
            self.faults.remove(i);
            return Err(object_store::Error::Generic {
                store: "FaultInjector",
                source: format!("injected failure of call #{nth} of {kind:?}").into(),
            });
        }
        Ok(())
    }
}

/// An [`ObjectStore`] that delegates to [`InMemory`] while recording
/// calls and optionally failing them; see the [module docs](self).
///
/// Pass `&injector` anywhere a `&dyn ObjectStore` is wanted—the `sync.rs`
/// transfer functions take exactly that.
pub struct ObjectStoreFaultInjector {
    inner: InMemory,
    state: Arc<Mutex<State>>,
}

impl ObjectStoreFaultInjector {
    /// Wrap an in-memory store.
    pub fn new(inner: InMemory) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// The wrapped store, for seeding fixtures and checking contents.
    /// Direct access is *not* recorded.
    pub fn inner(&self) -> &InMemory {
        &self.inner
    }

    /// Arm a fault: the `nth` call (1-based) of `kind` will fail with a
    /// canned error.
    pub fn fail_nth(&self, kind: CallKind, nth: usize) {
        self.lock().faults.push((kind, nth));
    }

    /// Every recorded call, in order.
    pub fn calls(&self) -> Vec<CallKind> {
        self.lock().log.clone()
    }

    /// Whether the log contains at least one call of `kind`.
    pub fn called(&self, kind: CallKind) -> bool {
        self.calls().contains(&kind)
    }

    fn record(&self, kind: CallKind) -> OsResult<()> {
        self.lock().record(kind)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("fault injector lock poisoned")
    }
}

#[async_trait]
impl ObjectStore for ObjectStoreFaultInjector {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> OsResult<PutResult> {
        self.record(CallKind::Put)?;
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> OsResult<Box<dyn MultipartUpload>> {
        self.record(CallKind::PutMultipart)?;
        let upload = self.inner.put_multipart_opts(location, opts).await?;
        Ok(Box::new(RecordingUpload {
            inner: upload,
            state: self.state.clone(),
        }))
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> OsResult<GetResult> {
        self.record(CallKind::Get)?;
        self.inner.get_opts(location, options).await
    }

    // Everything below is recorded only as the need arises; for now it
    // delegates silently.

    fn delete_stream(
        &self,
        locations: BoxStream<'static, OsResult<Path>>,
    ) -> BoxStream<'static, OsResult<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, OsResult<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> OsResult<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> OsResult<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

impl fmt::Debug for ObjectStoreFaultInjector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectStoreFaultInjector")
            .field("calls", &self.calls())
            .finish()
    }
}

impl fmt::Display for ObjectStoreFaultInjector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FaultInjector({})", self.inner)
    }
}

/// Wraps the uploads the injector hands out so their lifecycle lands in
/// the same log—`abort()` is a method on the upload, not on the store.
#[derive(Debug)]
struct RecordingUpload {
    inner: Box<dyn MultipartUpload>,
    state: Arc<Mutex<State>>,
}

#[async_trait]
impl MultipartUpload for RecordingUpload {
    fn put_part(&mut self, payload: PutPayload) -> UploadPart {
        let result = self
            .state
            .lock()
            .expect("fault injector lock poisoned")
            .record(CallKind::PutPart);
        match result {
            Ok(()) => self.inner.put_part(payload),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    async fn complete(&mut self) -> OsResult<PutResult> {
        self.state
            .lock()
            .expect("fault injector lock poisoned")
            .record(CallKind::Complete)?;
        self.inner.complete().await
    }

    async fn abort(&mut self) -> OsResult<()> {
        self.state
            .lock()
            .expect("fault injector lock poisoned")
            .record(CallKind::Abort)?;
        self.inner.abort().await
    }
}

#[cfg(test)]
mod test {
    use object_store::ObjectStoreExt;

    use super::*;
    use crate::prelude::*;

    // Both `super::*` and the prelude glob export a `Path`; the explicit
    // import shadows both and keeps `Path::from` the object_store type.
    use object_store::path::Path;

    /// The log distinguishes upload paths—which is what the CS-4 branch
    /// test needs—and covers the lifecycle of a handed-out upload.
    #[tokio::test]
    async fn test_records_calls() -> Result<()> {
        let injector = ObjectStoreFaultInjector::new(InMemory::default());

        injector
            .put(&Path::from("small.txt"), PutPayload::from_static(b"hi"))
            .await?;
        let mut upload = injector.put_multipart(&Path::from("big.bin")).await?;
        upload.put_part(PutPayload::from_static(b"part")).await?;
        upload.complete().await?;
        injector.get(&Path::from("small.txt")).await?;

        assert_eq!(
            injector.calls(),
            vec![
                CallKind::Put,
                CallKind::PutMultipart,
                CallKind::PutPart,
                CallKind::Complete,
                CallKind::Get,
            ],
        );

        Ok(())
    }

    /// An armed fault fails exactly the Nth call of its kind, exactly
    /// once—and a failed part can be driven all the way to `abort()`,
    /// which is the shape of the CS-6 test.
    #[tokio::test]
    async fn test_fails_nth_call_once() -> Result<()> {
        let injector = ObjectStoreFaultInjector::new(InMemory::default());

        injector.fail_nth(CallKind::PutPart, 2);
        let mut upload = injector.put_multipart(&Path::from("f.bin")).await?;
        upload.put_part(PutPayload::from_static(b"one")).await?;
        let err = upload
            .put_part(PutPayload::from_static(b"two"))
            .await
            .expect_err("the 2nd part was scheduled to fail");
        assert!(
            format!("{err}").contains("injected failure of call #2"),
            "the error should name the injected fault, got {err}",
        );
        upload.abort().await?;
        assert!(
            injector.called(CallKind::Abort),
            "abort must appear in the call log, got {:?}",
            injector.calls(),
        );

        // The fault fired once: further `put` calls succeed.
        injector.fail_nth(CallKind::Put, 1);
        assert!(
            injector
                .put(&Path::from("a"), PutPayload::from_static(b"a"))
                .await
                .is_err()
        );
        injector
            .put(&Path::from("b"), PutPayload::from_static(b"b"))
            .await?;

        Ok(())
    }
}
