//! Content-addressed storage: `ObjId -> bytes`.
//!
//! The memo table stores an [`ObjId`], never an output, so a row is small and
//! two rows agreeing on an answer share one copy of it.
//!
//! `put` and `get` both take `&self`. A content-addressed write is idempotent
//! and commutes with every other write, so exclusive access buys nothing, and
//! demanding `&mut self` here would force the eventual prolly-tree store to
//! hand out an exclusive handle it does not need. The in-memory implementation
//! pays for that with a mutex; the directory one does not need one at all.

use crate::error::{KernelError, Result};
use crate::id::ObjId;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// A store of immutable objects addressed by their content.
///
/// Implementations must satisfy: `get(put(b)) == Some(b)`, and `put` returns
/// [`ObjId::of(b)`] for every `b`. Everything else in the kernel is written
/// against those two sentences.
///
/// [`ObjId::of(b)`]: ObjId::of
pub trait Cas {
    /// Store bytes and return their address. Storing something already stored
    /// is a no-op that returns the same address.
    fn put(&self, bytes: &[u8]) -> Result<ObjId>;

    /// Load an object, or `None` if this store does not have it. Absence is
    /// not an error: a store is allowed to be a partial view, which is what
    /// makes a cache one of these.
    fn get(&self, id: ObjId) -> Result<Option<Vec<u8>>>;

    /// Whether the object is present. Overridable because a real store can
    /// usually answer this without moving the bytes.
    fn has(&self, id: ObjId) -> Result<bool> {
        Ok(self.get(id)?.is_some())
    }

    /// Load an object and check that it hashes to its address. A directory
    /// store names files by address and does not re-hash on read, so a
    /// truncated or swapped file would otherwise be decoded as though the
    /// address had vouched for it. One body for every consumer, so none can
    /// forget the check or spell the failure its own way.
    fn get_verified(&self, id: ObjId) -> Result<Verified> {
        Ok(match self.get(id)? {
            None => Verified::Missing,
            Some(bytes) if ObjId::of(&bytes) == id => Verified::Found(bytes),
            Some(_) => Verified::Corrupt,
        })
    }
}

/// What [`Cas::get_verified`] found at an address.
#[derive(Debug)]
pub enum Verified {
    /// The store does not have it: a miss, never an error.
    Missing,
    /// The bytes, which hash to the address.
    Found(Vec<u8>),
    /// Bytes are there and do not hash to the address: the store is damaged
    /// at this object, which the caller reports and treats as a miss.
    Corrupt,
}

/// In-memory store. For tests and for anything whose objects should not
/// outlive the process.
#[derive(Debug, Default)]
pub struct MemoryCas {
    objects: Mutex<BTreeMap<ObjId, Vec<u8>>>,
}

impl MemoryCas {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.objects().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects().is_empty()
    }

    /// Recovering from a poisoned lock is sound here because the only code
    /// inside the lock is a `BTreeMap` insert or lookup: there is no
    /// half-applied state a panicking thread could have left behind, and
    /// refusing to serve a cache because an unrelated thread panicked would
    /// turn one failure into a permanent one.
    fn objects(&self) -> std::sync::MutexGuard<'_, BTreeMap<ObjId, Vec<u8>>> {
        self.objects.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Cas for MemoryCas {
    fn put(&self, bytes: &[u8]) -> Result<ObjId> {
        let id = ObjId::of(bytes);
        self.objects().entry(id).or_insert_with(|| bytes.to_vec());
        Ok(id)
    }

    fn get(&self, id: ObjId) -> Result<Option<Vec<u8>>> {
        Ok(self.objects().get(&id).cloned())
    }

    fn has(&self, id: ObjId) -> Result<bool> {
        Ok(self.objects().contains_key(&id))
    }
}

/// Directory-backed store: one file per object, named by its address.
///
/// Flat rather than sharded. Sharding is a filesystem-performance decision
/// that depends on the object count, and this store is a stand-in until the
/// prolly-tree store lands, so adding a fan-out now would be guessing.
///
/// Writes go to a unique temporary file and are renamed into place, so a
/// reader never sees a partly written object and two concurrent writers of the
/// same object cannot interleave. An existing file is compared with the bytes
/// being put and replaced if it is damaged; `put` must restore
/// `get(put(b)) == Some(b)`, not trust a filename left by a crash or external
/// corruption. Neither the file nor the directory is fsynced: after a crash an
/// object may be absent, which is a miss and re-performs.
#[derive(Clone, Debug)]
pub struct DirCas {
    root: PathBuf,
}

/// Distinguishes temporary files written by this process from each other; the
/// pid alone is not enough because one process writes many objects at once.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl DirCas {
    /// Open (creating if needed) a store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| {
            KernelError::io(format!("creating store {}", root.display()), source)
        })?;
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, id: ObjId) -> PathBuf {
        self.root.join(id.hash().to_hex())
    }

    /// The one read body. `after_prefix` runs once the first bytes are in,
    /// which is how the atomic-reader test interleaves a concurrent writer;
    /// production passes a no-op and so runs the same code the test proves.
    fn read_object(&self, id: ObjId, after_prefix: impl FnOnce()) -> Result<Option<Vec<u8>>> {
        let path = self.object_path(id);
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(KernelError::io(
                    format!("reading {}", path.display()),
                    source,
                ));
            }
        };
        let mut bytes = Vec::new();
        let mut prefix = [0_u8; 4];
        match std::io::Read::read_exact(&mut file, &mut prefix) {
            Ok(()) => bytes.extend_from_slice(&prefix),
            // Shorter than the prefix: read whatever is there below.
            Err(source) if source.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(source) => {
                return Err(KernelError::io(
                    format!("reading {}", path.display()),
                    source,
                ));
            }
        }
        after_prefix();
        std::io::Read::read_to_end(&mut file, &mut bytes)
            .map_err(|source| KernelError::io(format!("reading {}", path.display()), source))?;
        Ok(Some(bytes))
    }

    #[cfg(test)]
    fn get_with_read_hook(
        &self,
        id: ObjId,
        after_prefix: impl FnOnce(),
    ) -> Result<Option<Vec<u8>>> {
        self.read_object(id, after_prefix)
    }

    /// Whether the file at `path` holds exactly `bytes`, compared through a
    /// fixed buffer: a duplicate `put` of a large object must not load the
    /// existing copy whole, and a corrupt oversized file must not become an
    /// allocation.
    fn holds(path: &Path, bytes: &[u8]) -> Result<bool> {
        let io = |source| KernelError::io(format!("comparing {}", path.display()), source);
        let mut file = fs::File::open(path).map_err(io)?;
        let mut buffer = [0_u8; 64 * 1024];
        let mut offset = 0usize;
        loop {
            let read = std::io::Read::read(&mut file, &mut buffer).map_err(io)?;
            if read == 0 {
                return Ok(offset == bytes.len());
            }
            let Some(expected) = bytes.get(offset..offset.saturating_add(read)) else {
                return Ok(false);
            };
            // `read` never exceeds the buffer (`Read::read`'s contract), and
            // `expected` is exactly `read` bytes when it exists; the checked
            // slice is the shape the workspace's `indexing_slicing` deny asks
            // for, not a case that can fail.
            if buffer.get(..read) != Some(expected) {
                return Ok(false);
            }
            offset = offset.saturating_add(read);
        }
    }
}

impl Cas for DirCas {
    fn put(&self, bytes: &[u8]) -> Result<ObjId> {
        let id = ObjId::of(bytes);
        let final_path = self.object_path(id);
        // The length first, then the bytes through a bounded buffer: the
        // common case is a duplicate put of an object already there, and it
        // must not cost a whole second copy in memory.
        match fs::metadata(&final_path) {
            Ok(metadata) if metadata.len() == bytes.len() as u64 => {
                if Self::holds(&final_path, bytes)? {
                    return Ok(id);
                }
            }
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(KernelError::io(
                    format!("reading {} before storing", final_path.display()),
                    source,
                ));
            }
        }

        let temp_path = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = fs::File::create(&temp_path).map_err(|source| {
            KernelError::io(format!("creating {}", temp_path.display()), source)
        })?;
        let written = file
            .write_all(bytes)
            .and_then(|()| file.flush())
            .map_err(|source| KernelError::io(format!("writing {}", temp_path.display()), source));
        drop(file);
        if let Err(error) = written {
            // Best effort: the object is not addressable, so a leftover
            // temporary is the only trace, and failing to remove it must not
            // mask the write error that caused it.
            drop(fs::remove_file(&temp_path));
            return Err(error);
        }

        fs::rename(&temp_path, &final_path).map_err(|source| {
            drop(fs::remove_file(&temp_path));
            KernelError::io(format!("publishing {}", final_path.display()), source)
        })?;
        Ok(id)
    }

    fn get(&self, id: ObjId) -> Result<Option<Vec<u8>>> {
        self.read_object(id, || {})
    }

    fn has(&self, id: ObjId) -> Result<bool> {
        Ok(self.object_path(id).exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(store: &dyn Cas) -> Result<()> {
        let id = store.put(b"payload")?;
        assert_eq!(id, ObjId::of(b"payload"));
        assert_eq!(store.get(id)?, Some(b"payload".to_vec()));
        assert!(store.has(id)?);
        // Absence is a `None`, not an error.
        assert_eq!(store.get(ObjId::of(b"absent"))?, None);
        assert!(!store.has(ObjId::of(b"absent"))?);
        // Re-putting is a no-op returning the same address.
        assert_eq!(store.put(b"payload")?, id);
        Ok(())
    }

    #[test]
    fn memory_store_round_trips() -> Result<()> {
        round_trip(&MemoryCas::new())
    }

    #[test]
    fn directory_store_round_trips() -> Result<()> {
        let dir = temp_dir("round-trip");
        let store = DirCas::open(&dir)?;
        let result = round_trip(&store);
        drop(fs::remove_dir_all(&dir));
        result
    }

    #[test]
    fn empty_object_is_storable() -> Result<()> {
        let store = MemoryCas::new();
        let id = store.put(b"")?;
        assert_eq!(store.get(id)?, Some(Vec::new()));
        assert_eq!(store.len(), 1);
        Ok(())
    }

    #[test]
    fn stored_objects_survive_reopening_the_directory() -> Result<()> {
        let dir = temp_dir("reopen");
        let id = DirCas::open(&dir)?.put(b"durable")?;
        let reopened = DirCas::open(&dir)?;
        let found = reopened.get(id)?;
        drop(fs::remove_dir_all(&dir));
        assert_eq!(found, Some(b"durable".to_vec()));
        Ok(())
    }

    #[test]
    fn putting_an_object_repairs_a_damaged_existing_file() -> Result<()> {
        let dir = temp_dir("repair");
        let store = DirCas::open(&dir)?;
        let id = store.put(b"durable")?;
        fs::write(store.object_path(id), b"damaged")
            .map_err(|source| KernelError::io("damaging the object file", source))?;

        assert_eq!(store.put(b"durable")?, id);
        assert_eq!(store.get(id)?, Some(b"durable".to_vec()));

        drop(fs::remove_dir_all(&dir));
        Ok(())
    }

    #[test]
    fn repair_is_atomic_for_a_reader_already_reading_the_old_object() -> Result<()> {
        let dir = temp_dir("repair-reader");
        let store = DirCas::open(&dir)?;
        let repaired = b"complete repaired payload";
        let damaged = b"damaged old payload";
        let id = store.put(repaired)?;
        fs::write(store.object_path(id), damaged)
            .map_err(|source| KernelError::io("damaging the object file", source))?;

        let reader_at_prefix = std::sync::Arc::new(std::sync::Barrier::new(2));
        let repair_finished = std::sync::Arc::new(std::sync::Barrier::new(2));
        let reader_store = store.clone();
        let reader_at_prefix_in_thread = reader_at_prefix.clone();
        let repair_finished_in_thread = repair_finished.clone();
        let reader = std::thread::spawn(move || {
            reader_store.get_with_read_hook(id, || {
                reader_at_prefix_in_thread.wait();
                repair_finished_in_thread.wait();
            })
        });

        reader_at_prefix.wait();
        let repair = store.put(repaired);
        repair_finished.wait();
        assert_eq!(repair?, id);
        let observed = reader
            .join()
            .expect("the concurrent CAS reader panicked")?
            .expect("the object disappeared during repair");
        assert!(
            observed.as_slice() == damaged || observed.as_slice() == repaired,
            "reader observed partial repair bytes: {observed:?}"
        );
        assert_eq!(store.get(id)?, Some(repaired.to_vec()));

        drop(fs::remove_dir_all(&dir));
        Ok(())
    }

    /// Only object files, so a directory listing is a listing of addresses.
    #[test]
    fn writing_leaves_no_temporary_behind() -> Result<()> {
        let dir = temp_dir("no-temp");
        let store = DirCas::open(&dir)?;
        store.put(b"one")?;
        store.put(b"two")?;
        let mut names: Vec<String> = fs::read_dir(&dir)
            .map_err(|source| KernelError::io("listing", source))?
            .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        names.sort();
        drop(fs::remove_dir_all(&dir));
        let mut expected = vec![
            ObjId::of(b"one").hash().to_hex(),
            ObjId::of(b"two").hash().to_hex(),
        ];
        expected.sort();
        assert_eq!(names, expected);
        Ok(())
    }

    fn temp_dir(label: &str) -> PathBuf {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ix-kernel-cas-{label}-{}-{unique}",
            std::process::id()
        ))
    }
}
