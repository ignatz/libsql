use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use libsql_sys::ffi::Sqlite3DbHeader;
use parking_lot::{Condvar, Mutex};
use rand::Rng;
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::task::JoinSet;
use uuid::Uuid;
use zerocopy::{AsBytes, FromZeroes};

use crate::checkpointer::CheckpointMessage;
use crate::error::Result;
use crate::io::file::FileExt;
use crate::io::{Io, StdIO};
use crate::replication::storage::StorageReplicator;
use crate::segment::list::SegmentList;
use crate::segment::Segment;
use crate::segment::{current::CurrentSegment, sealed::SealedSegment};
use crate::shared_wal::{SharedWal, SwapLog};
use crate::storage::{OnStoreCallback, Storage};
use crate::transaction::TxGuard;
use crate::{LibsqlFooter, LIBSQL_PAGE_SIZE};
use libsql_sys::name::NamespaceName;

enum Slot<IO: Io> {
    Wal(Arc<SharedWal<IO>>),
    /// Only a single thread is allowed to instantiate the wal. The first thread to acquire an
    /// entry in the registry map puts a building slot. Other connections will wait for the mutex
    /// to turn to true, after the slot has been updated to contain the wal
    Building(Arc<(Condvar, Mutex<bool>)>, Arc<Notify>),
}

/// Wal Registry maintains a set of shared Wal, and their respective set of files.
pub struct WalRegistry<IO: Io, S> {
    io: Arc<IO>,
    path: PathBuf,
    shutdown: AtomicBool,
    opened: DashMap<NamespaceName, Slot<IO>>,
    storage: Arc<S>,
    checkpoint_notifier: mpsc::Sender<CheckpointMessage>,
}

impl<S> WalRegistry<StdIO, S> {
    pub fn new(
        path: PathBuf,
        storage: Arc<S>,
        checkpoint_notifier: mpsc::Sender<CheckpointMessage>,
    ) -> Result<Self> {
        Self::new_with_io(StdIO(()), path, storage, checkpoint_notifier)
    }
}

impl<IO: Io, S> WalRegistry<IO, S> {
    pub fn new_with_io(
        io: IO,
        path: PathBuf,
        storage: Arc<S>,
        checkpoint_notifier: mpsc::Sender<CheckpointMessage>,
    ) -> Result<Self> {
        io.create_dir_all(&path)?;
        let registry = Self {
            io: io.into(),
            path,
            opened: Default::default(),
            shutdown: Default::default(),
            storage,
            checkpoint_notifier,
        };

        Ok(registry)
    }

    pub async fn get_async(&self, namespace: &NamespaceName) -> Option<Arc<SharedWal<IO>>> {
        loop {
            let notify = {
                match self.opened.get(namespace).as_deref() {
                    Some(Slot::Wal(wal)) => return Some(wal.clone()),
                    Some(Slot::Building(_, notify)) => notify.clone(),
                    None => return None,
                }
            };

            notify.notified().await
        }
    }
}

impl<IO, S> SwapLog<IO> for WalRegistry<IO, S>
where
    IO: Io,
    S: Storage<Segment = SealedSegment<IO::File>>,
{
    #[tracing::instrument(skip_all)]
    fn swap_current(&self, shared: &SharedWal<IO>, tx: &TxGuard<<IO as Io>::File>) -> Result<()> {
        assert!(tx.is_commited());
        // at this point we must hold a lock to a commited transaction.

        let current = shared.current.load();
        if current.is_empty() {
            return Ok(());
        }
        let start_frame_no = current.next_frame_no();
        let path = self
            .path
            .join(shared.namespace().as_str())
            .join(format!("{}:{start_frame_no:020}.seg", shared.namespace()));

        let segment_file = self.io.open(true, true, true, &path)?;
        let salt = self.io.with_rng(|rng| rng.gen());
        let new = CurrentSegment::create(
            segment_file,
            path,
            start_frame_no,
            current.db_size(),
            current.tail().clone(),
            salt,
            current.log_id(),
        )?;
        // sealing must the last fallible operation, because we don't want to end up in a situation
        // where the current log is sealed and it wasn't swapped.
        if let Some(sealed) = current.seal()? {
            // todo: pass config override here
            let notifier = self.checkpoint_notifier.clone();
            let namespace = shared.namespace().clone();
            let durable_frame_no = shared.durable_frame_no.clone();
            let cb: OnStoreCallback = Box::new(move |fno| {
                Box::pin(async move {
                    update_durable(fno, notifier, durable_frame_no, namespace).await;
                })
            });
            new.tail().push(sealed.clone());
            self.storage.store(&shared.namespace, sealed, None, cb);
        }

        shared.current.swap(Arc::new(new));
        tracing::debug!("current segment swapped");

        Ok(())
    }
}

async fn update_durable(
    new_durable: u64,
    notifier: mpsc::Sender<CheckpointMessage>,
    durable_frame_no_slot: Arc<Mutex<u64>>,
    namespace: NamespaceName,
) {
    {
        let mut g = durable_frame_no_slot.lock();
        if *g < new_durable {
            *g = new_durable;
        }
    }
    let _ = notifier.send(CheckpointMessage::Namespace(namespace)).await;
}

impl<IO, S> WalRegistry<IO, S>
where
    IO: Io,
    S: Storage<Segment = SealedSegment<IO::File>>,
{
    #[tracing::instrument(skip(self))]
    pub fn open(
        self: Arc<Self>,
        db_path: &Path,
        namespace: &NamespaceName,
    ) -> Result<Arc<SharedWal<IO>>> {
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(crate::error::Error::ShuttingDown);
        }

        loop {
            if let Some(entry) = self.opened.get(namespace) {
                match &*entry {
                    Slot::Wal(wal) => return Ok(wal.clone()),
                    Slot::Building(cond, _) => {
                        let cond = cond.clone();
                        cond.0
                            .wait_while(&mut cond.1.lock(), |ready: &mut bool| !*ready);
                        // the slot was updated: try again
                        continue;
                    }
                }
            }

            let action = match self.opened.entry(namespace.clone()) {
                dashmap::Entry::Occupied(e) => match e.get() {
                    Slot::Wal(shared) => return Ok(shared.clone()),
                    Slot::Building(wait, _) => Err(wait.clone()),
                },
                dashmap::Entry::Vacant(e) => {
                    let notifier = Arc::new((Condvar::new(), Mutex::new(false)));
                    let async_notifier = Arc::new(Notify::new());
                    e.insert(Slot::Building(notifier.clone(), async_notifier.clone()));
                    Ok((notifier, async_notifier))
                }
            };

            match action {
                Ok((notifier, async_notifier)) => {
                    // if try_open succedded, then the slot was updated and contains the shared wal, if it
                    // failed we need to remove the slot. Either way, notify all waiters
                    let ret = self.clone().try_open(&namespace, db_path);
                    if ret.is_err() {
                        self.opened.remove(namespace);
                    }

                    *notifier.1.lock() = true;
                    notifier.0.notify_all();
                    async_notifier.notify_waiters();

                    return ret;
                }
                Err(cond) => {
                    cond.0
                        .wait_while(&mut cond.1.lock(), |ready: &mut bool| !*ready);
                    // the slot was updated: try again
                    continue;
                }
            }
        }
    }

    fn try_open(
        self: Arc<Self>,
        namespace: &NamespaceName,
        db_path: &Path,
    ) -> Result<Arc<SharedWal<IO>>> {
        let db_file = self.io.open(false, true, true, db_path)?;
        let db_file_len = db_file.len()?;
        let header = if db_file_len > 0 {
            let mut header: Sqlite3DbHeader = Sqlite3DbHeader::new_zeroed();
            db_file.read_exact_at(header.as_bytes_mut(), 0)?;
            Some(header)
        } else {
            None
        };

        let footer = self.try_read_footer(&db_file)?;

        let mut checkpointed_frame_no = footer.map(|f| f.replication_index.get()).unwrap_or(0);

        let path = self.path.join(namespace.as_str());
        self.io.create_dir_all(&path)?;
        // TODO: handle that with abstract io
        let dir = walkdir::WalkDir::new(&path).sort_by_file_name().into_iter();

        // we only checkpoint durable frame_no so this is a good first estimate without an actual
        // network call.
        let durable_frame_no = Arc::new(Mutex::new(checkpointed_frame_no));

        let list = SegmentList::default();
        for entry in dir {
            let entry = entry.map_err(|e| e.into_io_error().unwrap())?;
            if entry
                .path()
                .extension()
                .map(|e| e.to_str().unwrap() != "seg")
                .unwrap_or(true)
            {
                continue;
            }

            let file = self.io.open(false, true, true, entry.path())?;

            if let Some(sealed) =
                SealedSegment::open(file.into(), entry.path().to_path_buf(), Default::default())?
            {
                let notifier = self.checkpoint_notifier.clone();
                let ns = namespace.clone();
                let durable_frame_no = durable_frame_no.clone();
                let cb: OnStoreCallback = Box::new(move |fno| {
                    Box::pin(async move {
                        update_durable(fno, notifier, durable_frame_no, ns).await;
                    })
                });
                // TODO: pass config override here
                self.storage.store(&namespace, sealed.clone(), None, cb);
                list.push(sealed);
            }
        }

        let log_id = match footer {
            Some(footer) if list.is_empty() => footer.log_id(),
            None if list.is_empty() => self.io.uuid(),
            Some(footer) => {
                let log_id = list
                    .with_head(|h| h.header().log_id.get())
                    .expect("non-empty list should have a head");
                let log_id = Uuid::from_u128(log_id);
                assert_eq!(log_id, footer.log_id());
                log_id
            }
            None => {
                let log_id = list
                    .with_head(|h| h.header().log_id.get())
                    .expect("non-empty list should have a head");
                Uuid::from_u128(log_id)
            }
        };

        let (db_size, next_frame_no) = list
            .with_head(|segment| {
                let header = segment.header();
                (header.size_after(), header.next_frame_no())
            })
            .unwrap_or_else(|| match header {
                Some(header) => (
                    header.db_size.get(),
                    NonZeroU64::new(header.replication_index.get() + 1)
                        .unwrap_or(NonZeroU64::new(1).unwrap()),
                ),
                None => (0, NonZeroU64::new(1).unwrap()),
            });

        let current_segment_path = path.join(format!("{namespace}:{next_frame_no:020}.seg"));

        let segment_file = self.io.open(true, true, true, &current_segment_path)?;
        let salt = self.io.with_rng(|rng| rng.gen());

        // if there is a tail, then the latest checkpointed frame_no is one before the the
        // start frame_no of the tail. We must read it from the tail, because a partial
        // checkpoint may have occured before a crash.
        if let Some(last) = tail.last() {
            checkpointed_frame_no = (last.start_frame_no() - 1).max(1)
        }

        let current = arc_swap::ArcSwap::new(Arc::new(CurrentSegment::create(
            segment_file,
            current_segment_path,
            next_frame_no,
            db_size,
            list.into(),
            salt,
            log_id,
        )?));

        let (new_frame_notifier, _) = tokio::sync::watch::channel(next_frame_no.get() - 1);

        let shared = Arc::new(SharedWal {
            current,
            wal_lock: Default::default(),
            db_file,
            registry: self.clone(),
            namespace: namespace.clone(),
            checkpointed_frame_no: checkpointed_frame_no.into(),
            new_frame_notifier,
            durable_frame_no,
            stored_segments: Box::new(StorageReplicator::new(
                self.storage.clone(),
                namespace.clone(),
            )),
            shutdown: false.into(),
            checkpoint_notifier: self.checkpoint_notifier.clone(),
            max_segment_size: 1000.into(),
            io: self.io.clone(),
        });

        self.opened
            .insert(namespace.clone(), Slot::Wal(shared.clone()));

        return Ok(shared);
    }

    fn try_read_footer(&self, db_file: &impl FileExt) -> Result<Option<LibsqlFooter>> {
        let len = db_file.len()?;
        if len as usize % LIBSQL_PAGE_SIZE as usize == size_of::<LibsqlFooter>() {
            let mut footer: LibsqlFooter = LibsqlFooter::new_zeroed();
            let footer_offset = (len / LIBSQL_PAGE_SIZE as u64) * LIBSQL_PAGE_SIZE as u64;
            db_file.read_exact_at(footer.as_bytes_mut(), footer_offset)?;
            footer.validate()?;
            Ok(Some(footer))
        } else {
            Ok(None)
        }
    }

    /// Attempts to sync all loaded dbs with durable storage
    pub async fn sync_all(&self) -> Result<()>
        where S: Storage,
    {
        tracing::info!("syncing {} namespaces", self.opened.len());
        for entry in self.opened.iter() {
            let Slot::Wal(shared) = entry.value() else { panic!("all wals should already be opened") };
            sync_one(shared, self.storage.as_ref()).await?;
        }

        Ok(())
    }

    // On shutdown, we checkpoint all the WALs. This require sealing the current segment, and when
    // checkpointing all the segments
    pub async fn shutdown(self: Arc<Self>) -> Result<()> {
        tracing::info!("shutting down registry");
        self.shutdown.store(true, Ordering::SeqCst);

        let mut join_set = JoinSet::<Result<()>>::new();
        let semaphore = Arc::new(Semaphore::new(8));
        for item in self.opened.iter() {
            let (name, slot) = item.pair();
            loop {
                match slot {
                    Slot::Wal(shared) => {
                        // acquire a permit or drain the join set
                        let permit = loop {
                            tokio::select! {
                                permit = semaphore.clone().acquire_owned() => break permit,
                                _ = join_set.join_next() => (),
                            }
                        };
                        let shared = shared.clone();
                        let name = name.clone();

                        join_set.spawn_blocking(move || {
                            let _permit = permit;
                            if let Err(e) = shared.shutdown() {
                                tracing::error!("error shutting down `{name}`: {e}");
                            }

                            Ok(())
                        });
                        break;
                    }
                    Slot::Building(_, notify) => {
                        // wait for shared to finish building
                        notify.notified().await;
                    }
                }
            }
        }

        while join_set.join_next().await.is_some() {}

        // we process any pending storage job, then checkpoint everything
        self.storage.shutdown().await;

        // wait for checkpointer to exit
        let _ = self
            .checkpoint_notifier
            .send(CheckpointMessage::Shutdown)
            .await;
        self.checkpoint_notifier.closed().await;

        tracing::info!("registry shutdown gracefully");

        Ok(())
    }
}

#[tracing::instrument(skip_all, fields(namespace = shared.namespace.as_str()))]
async fn sync_one<IO, S>(shared: &SharedWal<IO>, storage: &S) -> Result<()>
where IO: Io,
      S: Storage
{
    let remote_durable_frame_no = storage.durable_frame_no(&shared.namespace, None).await.map_err(Box::new)?;
    let local_current_frame_no = shared.current.load().next_frame_no().get() - 1;

    if remote_durable_frame_no >= local_current_frame_no {
        tracing::info!(remote_durable_frame_no, local_current_frame_no, "remote storage has newer segments");
    } else {
        tracing::info!("local database is up to date");
    }

    Ok(())
}

fn read_log_id_from_footer<F: FileExt>(db_file: &F, db_size: u64) -> io::Result<Uuid> {
    let mut footer: LibsqlFooter = LibsqlFooter::new_zeroed();
    let footer_offset = LIBSQL_PAGE_SIZE as u64 * db_size;
    // FIXME: failing to read the footer here is a sign of corrupted database: either we
    // have a tail to the segment list, or we have fully checkpointed the database. Can we
    // recover from that?
    db_file.read_exact_at(footer.as_bytes_mut(), footer_offset)?;
    Ok(footer.log_id())
}
