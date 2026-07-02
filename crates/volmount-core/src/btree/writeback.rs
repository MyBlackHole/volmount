use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::block_device::BlockDevice;
use crate::btree::bucket_io;
use crate::btree::node::BtreeNode;
use crate::journal::Journal;
use crate::types::StorageError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WritebackKey {
    block_addr: u64,
    seq: u64,
}

#[derive(Debug)]
struct WritebackTask {
    key: WritebackKey,
    node: Arc<BtreeNode>,
    backend: Arc<dyn BlockDevice>,
    journal: Arc<Journal>,
}

#[derive(Debug)]
struct WritebackInner {
    pending: AtomicUsize,
    closed: AtomicBool,
    last_error: Mutex<Option<StorageError>>,
    pending_keys: Mutex<HashSet<WritebackKey>>,
    wait_lock: Mutex<()>,
    wait_cv: Condvar,
}

impl WritebackInner {
    fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            last_error: Mutex::new(None),
            pending_keys: Mutex::new(HashSet::new()),
            wait_lock: Mutex::new(()),
            wait_cv: Condvar::new(),
        }
    }

    fn set_error(&self, err: StorageError) {
        let mut guard = self.last_error.lock().unwrap();
        if guard.is_none() {
            *guard = Some(err);
        }
        self.closed.store(true, Ordering::Release);
    }

    fn finish_one(&self, key: WritebackKey) {
        self.pending.fetch_sub(1, Ordering::AcqRel);
        self.pending_keys.lock().unwrap().remove(&key);
        let guard = self.wait_lock.lock().unwrap();
        self.wait_cv.notify_all();
        drop(guard);
    }
}

#[derive(Debug)]
pub struct WritebackHandle {
    inner: Arc<WritebackInner>,
    sender: Mutex<Option<Sender<WritebackTask>>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl WritebackHandle {
    pub fn new() -> Arc<Self> {
        let inner = Arc::new(WritebackInner::new());
        let (tx, rx) = mpsc::channel::<WritebackTask>();
        let worker_inner = inner.clone();
        let join = std::thread::spawn(move || worker_loop(worker_inner, rx));
        Arc::new(Self {
            inner,
            sender: Mutex::new(Some(tx)),
            join: Mutex::new(Some(join)),
        })
    }

    pub fn enqueue(
        &self,
        node: Arc<BtreeNode>,
        backend: Arc<dyn BlockDevice>,
        journal: Arc<Journal>,
    ) -> Result<(), StorageError> {
        let block_addr = node.block_addr();
        if block_addr == 0 {
            return Err(StorageError::InvalidArgument(
                "btree node has no bound physical block address".into(),
            ));
        }
        let key = WritebackKey {
            block_addr,
            seq: node.journal_seq,
        };
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(StorageError::JournalError("writeback queue closed".into()));
        }

        let mut pending_keys = self.inner.pending_keys.lock().unwrap();
        if !pending_keys.insert(key) {
            return Ok(());
        }
        self.inner.pending.fetch_add(1, Ordering::AcqRel);
        drop(pending_keys);

        let task = WritebackTask {
            key,
            node,
            backend,
            journal,
        };
        let send_result = {
            let sender = self.sender.lock().unwrap();
            sender
                .as_ref()
                .ok_or_else(|| StorageError::JournalError("writeback queue closed".into()))?
                .send(task)
        };

        if let Err(err) = send_result {
            self.inner.pending.fetch_sub(1, Ordering::AcqRel);
            self.inner.pending_keys.lock().unwrap().remove(&key);
            return Err(StorageError::JournalError(format!(
                "failed to enqueue writeback: {err}"
            )));
        }

        Ok(())
    }

    pub fn wait_idle(&self) -> Result<(), StorageError> {
        let mut guard = self.inner.wait_lock.lock().unwrap();
        while self.inner.pending.load(Ordering::Acquire) != 0 {
            guard = self.inner.wait_cv.wait(guard).unwrap();
        }
        if let Some(err) = self.inner.last_error.lock().unwrap().take() {
            return Err(err);
        }
        Ok(())
    }

    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        self.sender.lock().unwrap().take();
        if let Some(join) = self.join.lock().unwrap().take() {
            let _ = join.join();
        }
    }
}

fn worker_loop(inner: Arc<WritebackInner>, rx: Receiver<WritebackTask>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("writeback worker runtime");

    while let Ok(task) = rx.recv() {
        let result = rt.block_on(async {
            bucket_io::write_node_to_bucket(
                task.node.as_ref(),
                task.key.block_addr,
                task.backend.as_ref(),
            )
            .await
        });

        if result.is_ok() {
            if let Some(pin) = task.node.journal_pin.lock().unwrap().as_ref() {
                if pin.seq.load(Ordering::Acquire) == task.key.seq {
                    task.journal.bch2_journal_pin_drop(pin);
                }
            }
        } else if let Err(err) = result {
            inner.set_error(err);
        }

        inner.finish_one(task.key);
        if inner.closed.load(Ordering::Acquire) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::key::{BtreeKey, KeyType};
    use crate::btree::node::BtreeNode;
    use crate::journal::reclaim::{JournalEntryPinList, JournalPinType};
    use crate::journal::Journal;

    #[tokio::test]
    async fn test_writeback_enqueue_and_wait_idle() {
        let backend = Arc::new(MockBlockDevice::new());
        let journal = Arc::new(Journal::new(vec![100]));
        unsafe {
            assert!((*journal.pin_fifo.get())
                .push_back(JournalEntryPinList::new(1))
                .is_ok());
        }
        let handle = WritebackHandle::new();

        let mut node = Arc::new(BtreeNode::new_leaf());
        if let Some(node_mut) = Arc::get_mut(&mut node) {
            node_mut.set_block_addr(42);
            node_mut.journal_seq = 1;
            node_mut.insert(
                BtreeKey::new(1, 1, KeyType::Normal),
                crate::btree::BchVal::new(1, 1),
            );
            node_mut.compact();
        }
        {
            let mut pin = node.journal_pin.lock().unwrap();
            *pin = Some(crate::journal::reclaim::JournalEntryPin::new(
                None,
                JournalPinType::Btree0,
            ));
        }
        {
            let pin = node.journal_pin.lock().unwrap();
            let pin = pin.as_ref().unwrap();
            journal.bch2_journal_pin_add(1, pin, None);
        }

        handle
            .enqueue(node.clone(), backend.clone(), journal.clone())
            .unwrap();
        handle.wait_idle().unwrap();

        let mut buf = vec![0u8; 4096];
        backend
            .read_block(crate::types::BlockAddr::new(42), &mut buf)
            .await
            .unwrap();
        assert_eq!(
            node.journal_pin
                .lock()
                .unwrap()
                .as_ref()
                .map(|pin| pin.seq.load(Ordering::Acquire)),
            Some(0)
        );
        handle.close();
    }
}
