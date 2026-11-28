//! The seam between a Sync and how it fetches pages.
//!
//! A [`SyncDriver`] only collects a Source's pages ([`SyncPages`]); the Sync task writes
//! them and records the Sync run. [`OriginDriver`] is the production adapter; tests use
//! `FakeDriver`.

use std::fmt::Debug;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use super::blocking;
use super::origin::check_private_origin;
use crate::error::Result;
use crate::ingest::persist::SyncPages;
use crate::ingest::{local, web, SourceKind, SourceRecord};

/// A driver's collection of one Source's pages, in flight.
pub(crate) type Collecting = Pin<Box<dyn Future<Output = Result<SyncPages>> + Send>>;

/// How a Sync fetches its pages.
pub(crate) trait SyncDriver: Debug + Send + Sync {
    fn collect(&self, source: SourceRecord, prune: bool) -> Collecting;
}

/// The folder walk for local Sources and the web crawl for web Sources. A web Origin is
/// checked against the Private-origin rule before every crawl.
#[derive(Debug)]
pub(crate) struct OriginDriver {
    pub(crate) allow_private_origins: bool,
}

impl SyncDriver for OriginDriver {
    fn collect(&self, source: SourceRecord, prune: bool) -> Collecting {
        let allow_private_origins = self.allow_private_origins;
        Box::pin(async move {
            match source.kind {
                SourceKind::Local => {
                    blocking(move || {
                        local::collect(
                            &source.name,
                            Path::new(&source.origin),
                            &source.include_patterns,
                            &source.exclude_patterns,
                            prune,
                        )
                    })
                    .await
                }
                SourceKind::Web => {
                    check_private_origin(&source.origin, allow_private_origins)?;
                    web::collect(&source, prune).await
                }
            }
        })
    }
}

#[cfg(test)]
pub(crate) use fake::FakeDriver;

#[cfg(test)]
mod fake {
    use std::sync::{Arc, Mutex};

    use tokio::sync::{mpsc, Semaphore};

    use super::{Collecting, SyncDriver};
    use crate::error::VaultError;
    use crate::ingest::persist::{MissingPagePolicy, PageIngestOutcome, SyncPages};
    use crate::ingest::{normalize_markdown, split_sections, SourceRecord};

    #[derive(Debug)]
    enum Step {
        Pages,
        Fail(String),
        Panic,
        Given(Mutex<Option<SyncPages>>),
    }

    /// A test driver: returns one page, fails, panics, or returns given pages, optionally
    /// after pausing at a [`DriverGate`]. It records each `on_written` report.
    #[derive(Debug, Clone)]
    pub(crate) struct FakeDriver {
        step: Arc<Step>,
        gate: Option<(mpsc::UnboundedSender<()>, Arc<Semaphore>)>,
        written: Arc<Mutex<Vec<bool>>>,
    }

    /// Holds a [`FakeDriver`]'s collection until [`open`](Self::open).
    pub(crate) struct DriverGate {
        reached: mpsc::UnboundedReceiver<()>,
        go: Arc<Semaphore>,
    }

    impl DriverGate {
        /// Wait until a Sync is inside the driver.
        pub(crate) async fn reached(&mut self) {
            tokio::time::timeout(std::time::Duration::from_secs(10), self.reached.recv())
                .await
                .expect("the Sync reaches the driver")
                .expect("driver alive");
        }

        /// Let one paused collection go on.
        pub(crate) fn open(&self) {
            self.go.add_permits(1);
        }
    }

    impl FakeDriver {
        fn with(step: Step) -> Self {
            Self {
                step: Arc::new(step),
                gate: None,
                written: Arc::default(),
            }
        }

        /// One page, `p.md`, every Sync.
        pub(crate) fn one_page() -> Self {
            Self::with(Step::Pages)
        }

        /// The walk or crawl fails with `message`.
        pub(crate) fn failing(message: &str) -> Self {
            Self::with(Step::Fail(message.to_string()))
        }

        /// The walk or crawl panics.
        pub(crate) fn panicking() -> Self {
            Self::with(Step::Panic)
        }

        /// `pages`, once.
        pub(crate) fn returning(pages: SyncPages) -> Self {
            Self::with(Step::Given(Mutex::new(Some(pages))))
        }

        /// Pause every collection until the returned gate opens.
        pub(crate) fn gated(mut self) -> (Self, DriverGate) {
            let (reached_tx, reached) = mpsc::unbounded_channel();
            let go = Arc::new(Semaphore::new(0));
            self.gate = Some((reached_tx, Arc::clone(&go)));
            (self, DriverGate { reached, go })
        }

        /// The `on_written` reports so far: `true` for a successful write.
        pub(crate) fn written(&self) -> Vec<bool> {
            self.written.lock().expect("written lock").clone()
        }
    }

    impl SyncDriver for FakeDriver {
        fn collect(&self, _source: SourceRecord, _prune: bool) -> Collecting {
            let this = self.clone();
            Box::pin(async move {
                if let Some((reached, go)) = &this.gate {
                    let _ = reached.send(());
                    go.acquire().await.expect("gate open").forget();
                }
                let mut pages = match &*this.step {
                    Step::Fail(message) => return Err(VaultError::Other(message.clone())),
                    Step::Panic => panic!("driver panicked mid-crawl"),
                    Step::Given(pages) => pages
                        .lock()
                        .expect("pages lock")
                        .take()
                        .expect("FakeDriver::returning collects once"),
                    Step::Pages => {
                        let doc = normalize_markdown("# P\n\nPage.\n", "p");
                        let chunks = split_sections(&doc.content_md);
                        SyncPages::new(
                            vec![PageIngestOutcome::Ready {
                                uri: "p.md".into(),
                                doc,
                                chunks,
                                etag: None,
                                last_modified: None,
                                http_status: None,
                            }],
                            MissingPagePolicy::MarkStale,
                        )
                    }
                };
                let written = Arc::clone(&this.written);
                let inner = pages.on_written.take();
                pages.on_written = Some(Box::new(move |ok, took| {
                    written.lock().expect("written lock").push(ok);
                    if let Some(inner) = inner {
                        inner(ok, took);
                    }
                }));
                Ok(pages)
            })
        }
    }
}
