use std::{path::{Path, PathBuf}, sync::Arc};

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use tokio::runtime::Runtime;
use crate::block::Block;
use crate::block_iterator::BlockIterator;

use crate::mem_table::MemTable;
use crate::sst::SsTableInfo;
use crate::tablestore::TableStore;

pub struct DbOptions {
  pub flush_ms: usize,
}

#[derive(Clone)]
pub(crate) struct DbState {
  pub(crate) memtable: Arc<MemTable>,
  pub(crate) imm_memtables: Vec<Arc<MemTable>>,
  pub(crate) l0: Vec<SsTableInfo>,
  pub(crate) next_sst_id: usize,
}

impl DbState {
  fn create() -> Self {
    Self {
      memtable: Arc::new(MemTable::new()),
      imm_memtables: Vec::new(),
      l0: Vec::new(),
      next_sst_id: 0,
    }
  }
}

pub(crate) struct DbInner {
  pub(crate) state: Arc<RwLock<Arc<DbState>>>,
  #[allow(dead_code)]  // TODO remove this once we write SSTs to disk
  pub(crate) path: PathBuf,
  pub(crate) options: DbOptions,
  pub(crate) table_store: TableStore,
  pub(crate) runtime: Arc<Runtime>,
}

// TODO Return Result<> for get/put/delete everything... ?
impl DbInner {
  pub fn new(
    path: impl AsRef<Path>,
    options: DbOptions,
    table_store: TableStore,
    runtime: Arc<Runtime>
  ) -> Self {
    let path_buf = path.as_ref().to_path_buf();

    if !path_buf.exists() {
      std::fs::create_dir_all(path).unwrap_or_else(|_| {
        panic!("Failed to create directory: {}", path_buf.display());
      });
    }

    //let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    Self {
      state: Arc::new(RwLock::new(Arc::new(DbState::create()))),
      path: path_buf,
      options,
      table_store,
      runtime: runtime.clone()
    }
  }

  /// Get the value for a given key.
  pub async fn get(&self, key: &[u8]) -> Option<Bytes> {
    let snapshot = {
      let guard = self.state.read();
      Arc::clone(&guard)
    };

    let maybe_bytes = std::iter::once(&snapshot.memtable)
      .chain(snapshot.imm_memtables.iter())
      .find_map(|memtable| {
        memtable.get(key)
      });

    // Filter is needed to remove tombstone deletes.
    if let Some(val) = maybe_bytes.filter(|value| !value.is_empty()) {
      return Some(val);
    }

    for sst in &snapshot.as_ref().l0 {
      if let Some(block_index) = self.find_block_for_key(sst, key) {
        let block = self.table_store.read_block(sst, block_index).await;
        if let Some(val) = self.find_val_in_block(&block, key) {
          if val.is_empty() {
            return None;
          }
          return Some(Bytes::from(val));
        }
      }
    }
    None
  }

  fn find_block_for_key(&self, sst: &SsTableInfo, key: &[u8]) -> Option<usize> {
    let block_idx = sst.block_meta.partition_point(|bm| bm.first_key > key);
    if block_idx == sst.block_meta.len() {
      return None
    }
    Some(block_idx)
  }

  fn find_val_in_block(&self, block: &Block, key: &[u8]) -> Option<Vec<u8>> {
    let mut iter = BlockIterator::from_first_key(block);
    while let Some(current_key) = iter.key() {
      if current_key == key {
        // TODO: there should be some way to do this without copying the buffer, but
        //       rust won't let me because iter goes out of scope
        return Some(Vec::from(iter.val().unwrap()))
      }
      iter.advance();
    }
    None
  }

  /// Put a key-value pair into the database. Key and value must not be empty.
  pub async fn put(&self, key: &[u8], value: &[u8]) {
    assert!(!key.is_empty(), "key cannot be empty");
    assert!(!value.is_empty(), "value cannot be empty");

    // Clone memtable to avoid a deadlock with flusher thread.
    let memtable = {
      let guard = self.state.read();
      Arc::clone(&guard.memtable)
    };

    memtable.put(key, value).await;
  }

  /// Delete a key from the database. Key must not be empty.
  pub async fn delete(&self, key: &[u8]) {
    assert!(!key.is_empty(), "key cannot be empty");

    // Clone memtable to avoid a deadlock with flusher thread.
    let memtable = {
      let guard = self.state.read();
      Arc::clone(&guard.memtable)
    };

    memtable.delete(key).await;
  }
}

pub struct Db {
  inner: Arc<DbInner>,
  /// Notifies the L0 flush thread to stop working.
  flush_notifier: crossbeam_channel::Sender<()>,
  /// The handle for the flush thread.
  flush_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Db {
  pub fn open(
    path: impl AsRef<Path>,
    options: DbOptions,
    table_store: TableStore,
    runtime: Arc<Runtime>
  ) -> Self {
    let inner = Arc::new(DbInner::new(path, options, table_store, runtime));
    let (tx, rx) = crossbeam_channel::unbounded();
    let flush_thread = inner.spawn_flush_thread(rx);
    Self {
      inner,
      flush_notifier: tx,
      flush_thread: Mutex::new(flush_thread),
    }
  }

  pub fn close(&self) {
    // Tell the notifier thread to shut down.
    self.flush_notifier.send(()).ok();

    // Wait for the flush thread to finish.
    let mut flush_thread = self.flush_thread.lock();
    if let Some(flush_thread) = flush_thread.take() {
        flush_thread
            .join()
            .expect("Failed to join flush thread");
    }

    // Force a final flush on the mutable memtable
    self.inner.flush();
  }

  pub async fn get(&self, key: &[u8]) -> Option<Bytes> {
    self.inner.get(key).await
  }

  pub async fn put(&self, key: &[u8], value: &[u8]) {
    // TODO move the put into an async block by blocking on the memtable flush
    self.inner.put(key, value).await;
  }

  pub async fn delete(&self, key: &[u8]) {
    // TODO move the put into an async block by blocking on the memtable flush
    self.inner.delete(key).await;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use object_store::memory::InMemory;
  use object_store::ObjectStore;
  use tokio::runtime::Runtime;

  #[test]
  fn test_put_get_delete() {
    let rt = Arc::new(Runtime::new().unwrap());
    rt.block_on(async {
      let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
      let table_store = TableStore::new(object_store);
      let kv_store = Db::open("/tmp/test_kv_store", DbOptions { flush_ms: 100 }, table_store, rt.clone());
      let key = b"test_key";
      let value = b"test_value";
      kv_store.put(key, value).await;
      assert_eq!(kv_store.get(key).await, Some(Bytes::from_static(value)));
      kv_store.delete(key).await;
      assert!(kv_store.get(key).await.is_none());
    });
  }
}