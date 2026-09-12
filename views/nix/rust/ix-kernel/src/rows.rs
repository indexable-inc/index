//! Memo rows that outlive the process.
//!
//! [`MemoTable`] is in memory, so a second run of the same build re-performs
//! everything the first one performed. The objects already survive, because
//! [`DirCas`] writes them to a directory; what does not survive is the
//! `(Domain, Key) -> ObjId` mapping that says which object answers which
//! request. This is that mapping on disk.
//!
//! # A row proves it belongs to its key
//!
//! The obvious layout, a file named by the key holding the output address, has
//! a failure this one does not: nothing ties the contents to the name. A row
//! copied, truncated at the wrong moment, or restored from a backup of a
//! different store would hand back an answer computed for some other request,
//! and every integrity check downstream would pass, because the object really
//! is the object its address names. It would simply be the wrong object.
//!
//! So a row carries the canonically encoded request that produced it, and
//! loading recomputes `Key::mint(domain, request)` and requires it to equal
//! the key the row was filed under. A row that fails is dropped and reported,
//! never used. The cost is that the request is stored twice, once here and
//! once inside whatever the object is; for a cache that is the right trade,
//! because the alternative is a wrong answer that looks exactly like a right
//! one.
//!
//! # What is deliberately not guaranteed
//!
//! Only [`Policy::Keyed`] rows belong here. Keyed means re-performing is
//! always safe, so every failure in this module degrades to a miss: a row that
//! will not parse, will not verify, or names an object the store does not
//! have is dropped, and the effect runs again. That is why this can be a
//! best-effort cache with no locking and no fsync. Pins are not stored here;
//! they have [`EffectLock`], which is a different discipline with different
//! obligations.
//!
//! [`DirCas`]: crate::cas::DirCas
//! [`EffectLock`]: crate::lock::EffectLock
//! [`Policy::Keyed`]: crate::table::Policy::Keyed

use crate::canon::{self, CanonValue};
use crate::error::{KernelError, Result};
use crate::id::{Domain, Key, ObjId};
use crate::table::{Entry, MemoTable, Policy, Provenance};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Why a row on disk was not used. Every variant is a miss, never an error the
/// caller has to handle, but each one is worth saying out loud: a store that
/// silently rejects every row is indistinguishable from a cold one, and the
/// difference is the whole value of the feature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// The file name was not a key.
    BadName { name: String },
    /// The bytes were not a canonical encoding, or not of a row.
    Malformed { key: String, detail: String },
    /// The stored request does not hash to the key the row is filed under.
    /// This is the check that stops a mis-filed row becoming a wrong answer.
    KeyMismatch { key: String, recomputed: String },
    /// The row names an object the store does not have.
    Dangling { key: String, output: String },
}

impl core::fmt::Display for Rejected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadName { name } => write!(f, "row file name {name:?} is not a key"),
            Self::Malformed { key, detail } => write!(f, "row {key} is malformed: {detail}"),
            Self::KeyMismatch { key, recomputed } => write!(
                f,
                "row {key} holds a request that hashes to {recomputed}; \
                 it was filed under the wrong key and has been ignored"
            ),
            Self::Dangling { key, output } => {
                write!(
                    f,
                    "row {key} names object {output}, which the store does not have"
                )
            }
        }
    }
}

/// The result of asking for one row.
///
/// A point lookup, not a scan. Rows are named by their key, so finding one is
/// opening a known path; loading the whole domain to answer a single question
/// costs the reader O(everything anybody ever cached) and was measured doing
/// exactly that, turning a warm store into a 3.4% pessimisation on a corpus of
/// cheap files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// No row for this key. A cold entry, not a problem.
    Missing,
    Found(ObjId),
    /// A row exists and cannot be trusted. The caller re-performs and should
    /// say why, or a store that refuses everything is silently a cold one.
    Refused(Rejected),
}

/// What a load did. Reported rather than logged here, because a library that
/// picks its own logging sink is a library the caller cannot quieten.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadReport {
    pub loaded: usize,
    pub rejected: Vec<Rejected>,
}

impl LoadReport {
    /// True when the directory held rows and every one of them was refused,
    /// which is the state most worth surfacing: it looks exactly like a cold
    /// cache from the outside and is usually a bug rather than a fresh start.
    #[must_use]
    pub fn all_rejected(&self) -> bool {
        self.loaded == 0 && !self.rejected.is_empty()
    }
}

/// One row, as an eviction policy sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowInfo {
    pub path: PathBuf,
    pub name: String,
    /// Last use, as recorded by [`DirRows::touch`]. A row never touched
    /// carries its write time, which is its first and only use so far.
    pub used: std::time::SystemTime,
    pub bytes: u64,
    /// The object this row names, or `None` if the row does not parse.
    pub output: Option<ObjId>,
}

/// A `.tmp-*` file: a write that was interrupted before its rename, or one
/// in flight right now. Which of the two is the caller's to know; a sweep that
/// holds the publication lock knows nothing is in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Temporary {
    pub path: PathBuf,
    pub bytes: u64,
}

/// What [`DirRows::inventory_all`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    pub rows: Vec<RowInfo>,
    pub temporaries: Vec<Temporary>,
}

/// Distinguishes temporary files written by this process from each other.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Directory-backed memo rows, one file per row, grouped by domain.
///
/// Writes go to a unique temporary file and are renamed into place, so a
/// reader never sees a partly written row. Nothing is fsynced: after a crash a
/// row may be absent, which is a miss, but it cannot be present and truncated.
#[derive(Clone, Debug)]
pub struct DirRows {
    root: PathBuf,
}

impl DirRows {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| {
            KernelError::io(format!("creating row store {}", root.display()), source)
        })?;
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn domain_dir(&self, domain: Domain) -> PathBuf {
        self.root.join(domain.hash().to_hex())
    }

    /// Read every row for a domain into `table`, dropping any that do not
    /// verify. `has_object` decides whether the store still holds an output,
    /// so a row pointing at a swept object is a miss rather than a promise the
    /// cache cannot keep.
    pub fn load(
        &self,
        domain: Domain,
        table: &mut MemoTable,
        has_object: &dyn Fn(ObjId) -> bool,
    ) -> Result<LoadReport> {
        let dir = self.domain_dir(domain);
        let mut report = LoadReport::default();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A domain nobody has written yet is a cold cache, not a failure.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(source) => {
                return Err(KernelError::io(
                    format!("reading {}", dir.display()),
                    source,
                ));
            }
        };

        for entry in entries {
            let entry = entry
                .map_err(|source| KernelError::io(format!("reading {}", dir.display()), source))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Temporaries from an interrupted write are not rows.
            if name.starts_with(".tmp-") {
                continue;
            }
            let Ok(hash) = crate::hash::Hash::from_hex(&name) else {
                report.rejected.push(Rejected::BadName { name });
                continue;
            };
            let key = Key::from_hash(hash);
            let bytes = match fs::read(entry.path()) {
                Ok(bytes) => bytes,
                Err(source) => {
                    report.rejected.push(Rejected::Malformed {
                        key: name,
                        detail: source.to_string(),
                    });
                    continue;
                }
            };
            match parse_row(&bytes) {
                Err(detail) => report
                    .rejected
                    .push(Rejected::Malformed { key: name, detail }),
                Ok((request, output)) => {
                    // The check the module header is about: a row must hash to
                    // the name it is filed under.
                    let recomputed = Key::mint(domain, &request);
                    if recomputed != key {
                        report.rejected.push(Rejected::KeyMismatch {
                            key: name,
                            recomputed: recomputed.to_string(),
                        });
                    } else if !has_object(output) {
                        report.rejected.push(Rejected::Dangling {
                            key: name,
                            output: output.to_string(),
                        });
                    } else {
                        table.insert(
                            domain,
                            key,
                            Entry {
                                output,
                                policy: Policy::Keyed,
                                provenance: Provenance::Deterministic,
                            },
                        );
                        report.loaded += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    /// Read one row, verifying it belongs to the key it was filed under.
    ///
    /// Every failure is [`Lookup::Refused`] or [`Lookup::Missing`], never an
    /// error: under `Keyed` the caller can always re-perform, and a cache that
    /// can fail a build because a file it wrote is damaged is worse than no
    /// cache.
    #[must_use]
    pub fn get(&self, domain: Domain, key: Key) -> Lookup {
        let name = key.hash().to_hex();
        let path = self.domain_dir(domain).join(&name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Lookup::Missing;
            }
            Err(source) => {
                return Lookup::Refused(Rejected::Malformed {
                    key: name,
                    detail: source.to_string(),
                });
            }
        };
        match parse_row(&bytes) {
            Err(detail) => Lookup::Refused(Rejected::Malformed { key: name, detail }),
            Ok((request, output)) => {
                // The check the module header is about, applied per lookup.
                let recomputed = Key::mint(domain, &request);
                if recomputed == key {
                    Lookup::Found(output)
                } else {
                    Lookup::Refused(Rejected::KeyMismatch {
                        key: name,
                        recomputed: recomputed.to_string(),
                    })
                }
            }
        }
    }

    /// Remove a row, for a caller that has found its answer unusable. `Ok(true)`
    /// when a row was there; a missing row is `Ok(false)`, not an error, since
    /// the state the caller wants is "no row" and that is now the case.
    pub fn remove(&self, domain: Domain, key: Key) -> Result<bool> {
        let path = self.domain_dir(domain).join(key.hash().to_hex());
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(KernelError::io(
                format!("removing {}", path.display()),
                source,
            )),
        }
    }

    /// Mark a row as used now, for whatever policy decides what to evict.
    ///
    /// Best effort and deliberately not an error: recency is an optimisation
    /// input, and a store on a read-only mount should still serve hits rather
    /// than fail them. Returns whether the mark landed, so a caller that cares
    /// can notice a store it will never be able to age correctly.
    ///
    /// Recency of *use* rather than of write, because the two disagree exactly
    /// where it matters. Under an edit loop the entries worth keeping are
    /// written once and then only read, while the churn is freshly written
    /// every round, so evicting by write time removes the working set and
    /// keeps the garbage.
    pub fn touch(&self, domain: Domain, key: Key) -> bool {
        let path = self.domain_dir(domain).join(key.hash().to_hex());
        // filetime without the dependency: opening for append and writing
        // nothing does not move mtime, so the portable move is a utimes call,
        // and std has no such thing. Rewriting the file would move it and cost
        // a full write; instead the mtime is set by opening with `File::open`
        // and calling `set_modified`, which is a metadata-only update.
        std::fs::File::open(&path)
            .and_then(|file| file.set_modified(std::time::SystemTime::now()))
            .is_ok()
    }

    /// Every row in a domain, as (key, last use, size in bytes), for a caller
    /// deciding what to evict. Rows that will not parse are included: they
    /// take space and are worth reclaiming first. Temporaries are left out;
    /// [`DirRows::inventory_all`] reports them.
    pub fn inventory(&self, domain: Domain) -> Result<Vec<RowInfo>> {
        let mut inventory = Inventory::default();
        self.inventory_dir(&self.domain_dir(domain), &mut inventory)?;
        Ok(inventory.rows)
    }

    /// Every row of every domain, plus the temporaries interrupted writes left
    /// behind, for a sweep that owns the whole store.
    ///
    /// # Nothing is guessed
    ///
    /// A row this cannot read is an error, not a row with no output. An
    /// eviction policy credits an object to the rows that name it, so a live
    /// row read as "names nothing" does not make the sweep conservative: it
    /// lets the sweep delete the object the row still points at and report a
    /// healthy store. The only failure tolerated is a file that vanished
    /// between listing and reading, which is a row somebody else already
    /// removed and so cannot be keeping anything alive.
    pub fn inventory_all(&self) -> Result<Inventory> {
        let mut inventory = Inventory::default();
        let read = match fs::read_dir(&self.root) {
            Ok(read) => read,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(inventory),
            Err(source) => {
                return Err(KernelError::io(
                    format!("reading {}", self.root.display()),
                    source,
                ));
            }
        };
        for entry in read {
            let entry = entry.map_err(|source| {
                KernelError::io(format!("reading {}", self.root.display()), source)
            })?;
            let file_type = entry.file_type().map_err(|source| {
                KernelError::io(format!("reading {}", entry.path().display()), source)
            })?;
            // Only domain directories live directly under the root.
            if file_type.is_dir() {
                self.inventory_dir(&entry.path(), &mut inventory)?;
            }
        }
        Ok(inventory)
    }

    fn inventory_dir(&self, dir: &Path, out: &mut Inventory) -> Result<()> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(KernelError::io(
                    format!("reading {}", dir.display()),
                    source,
                ));
            }
        };
        for entry in entries {
            let entry = entry
                .map_err(|source| KernelError::io(format!("reading {}", dir.display()), source))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(KernelError::io(
                        format!("reading {}", path.display()),
                        source,
                    ));
                }
            };
            if !metadata.is_file() {
                continue;
            }
            if name.starts_with(".tmp-") {
                out.temporaries.push(Temporary {
                    path,
                    bytes: metadata.len(),
                });
                continue;
            }
            let used = metadata.modified().map_err(|source| {
                KernelError::io(format!("reading the mtime of {}", path.display()), source)
            })?;
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(KernelError::io(
                        format!("reading row {}", path.display()),
                        source,
                    ));
                }
            };
            // A row that was read and will not parse still occupies space, and
            // its output is unknown, so it references nothing and sweeps first.
            let output = parse_row(&bytes).ok().map(|(_, output)| output);
            out.rows.push(RowInfo {
                path,
                name,
                used,
                bytes: metadata.len(),
                output,
            });
        }
        Ok(())
    }

    /// Write one row. Idempotent: the same request and output rewrite the same
    /// bytes, so two processes racing on one row cannot produce a third thing.
    pub fn put(&self, domain: Domain, request: &[u8], output: ObjId) -> Result<()> {
        let key = Key::mint(domain, request);
        let dir = self.domain_dir(domain);
        fs::create_dir_all(&dir)
            .map_err(|source| KernelError::io(format!("creating {}", dir.display()), source))?;
        let bytes = encode_row(request, output)?;

        let temp = dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = fs::File::create(&temp)
            .map_err(|source| KernelError::io(format!("creating {}", temp.display()), source))?;
        let written = file
            .write_all(&bytes)
            .and_then(|()| file.flush())
            .map_err(|source| KernelError::io(format!("writing {}", temp.display()), source));
        drop(file);
        if let Err(error) = written {
            drop(fs::remove_file(&temp));
            return Err(error);
        }
        let final_path = dir.join(key.hash().to_hex());
        fs::rename(&temp, &final_path).map_err(|source| {
            drop(fs::remove_file(&temp));
            KernelError::io(format!("publishing {}", final_path.display()), source)
        })
    }
}

fn encode_row(request: &[u8], output: ObjId) -> Result<Vec<u8>> {
    Ok(canon::encode(&CanonValue::map([
        ("req", CanonValue::Bytes(request.to_vec())),
        ("out", CanonValue::Bytes(output.hash().as_bytes().to_vec())),
    ]))?)
}

fn parse_row(bytes: &[u8]) -> core::result::Result<(Vec<u8>, ObjId), String> {
    let value = canon::decode(bytes).map_err(|e| e.to_string())?;
    let CanonValue::Map(entries) = value else {
        return Err("not a map".to_owned());
    };
    let field = |name: &str| {
        entries.iter().find_map(|(k, v)| match (k, v) {
            (CanonValue::Str(k), CanonValue::Bytes(v)) if k == name => Some(v.clone()),
            _ => None,
        })
    };
    let request = field("req").ok_or("no 'req' field")?;
    let raw = field("out").ok_or("no 'out' field")?;
    let raw: [u8; 32] = raw
        .as_slice()
        .try_into()
        .map_err(|_| format!("'out' is {} bytes, not 32", raw.len()))?;
    Ok((
        request,
        ObjId::from_hash(crate::hash::Hash::from_bytes(raw)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::{Cas, DirCas, MemoryCas};
    use std::sync::atomic::AtomicBool;

    fn temp_dir(label: &str) -> PathBuf {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ix-kernel-rows-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    fn always(_: ObjId) -> bool {
        true
    }

    #[test]
    fn a_written_row_loads_back() -> Result<()> {
        let dir = temp_dir("round-trip");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        let request = b"request".to_vec();
        let output = ObjId::of(b"answer");
        rows.put(domain, &request, output)?;

        let mut table = MemoTable::new();
        let report = rows.load(domain, &mut table, &always)?;
        drop(fs::remove_dir_all(&dir));

        assert_eq!(report.loaded, 1);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        let key = Key::mint(domain, &request);
        assert_eq!(table.get(domain, key).map(|e| e.output), Some(output));
        Ok(())
    }

    #[test]
    fn a_domain_never_written_is_a_cold_cache_not_an_error() -> Result<()> {
        let dir = temp_dir("cold");
        let rows = DirRows::open(&dir)?;
        let mut table = MemoTable::new();
        let report = rows.load(Domain::mint("e", "op"), &mut table, &always)?;
        drop(fs::remove_dir_all(&dir));
        assert_eq!(report, LoadReport::default());
        assert!(table.is_empty());
        Ok(())
    }

    /// The check this module exists for. A row moved to another key's name
    /// must be refused, because using it would answer one request with another
    /// request's answer and every downstream integrity check would pass.
    #[test]
    fn a_row_filed_under_the_wrong_key_is_refused() -> Result<()> {
        let dir = temp_dir("misfiled");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        rows.put(domain, b"request-one", ObjId::of(b"answer-one"))?;

        // Rename the row to the key of a different request, exactly what a
        // careless copy or a restored backup would produce.
        let domain_dir = dir.join(domain.hash().to_hex());
        let wrong = Key::mint(domain, b"request-two");
        let from = domain_dir.join(Key::mint(domain, b"request-one").hash().to_hex());
        let to = domain_dir.join(wrong.hash().to_hex());
        fs::rename(&from, &to).map_err(|s| KernelError::io("renaming", s))?;

        let mut table = MemoTable::new();
        let report = rows.load(domain, &mut table, &always)?;
        drop(fs::remove_dir_all(&dir));

        assert_eq!(report.loaded, 0);
        assert!(
            matches!(report.rejected.first(), Some(Rejected::KeyMismatch { .. })),
            "{:?}",
            report.rejected
        );
        assert!(table.is_empty(), "a mis-filed row reached the table");
        assert!(report.all_rejected());
        Ok(())
    }

    #[test]
    fn a_truncated_row_is_refused() -> Result<()> {
        let dir = temp_dir("truncated");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        rows.put(domain, b"request", ObjId::of(b"answer"))?;
        let path = dir
            .join(domain.hash().to_hex())
            .join(Key::mint(domain, b"request").hash().to_hex());
        let bytes = fs::read(&path).map_err(|s| KernelError::io("reading", s))?;
        fs::write(&path, bytes.get(..bytes.len() / 2).unwrap_or(&[]))
            .map_err(|s| KernelError::io("truncating", s))?;

        let mut table = MemoTable::new();
        let report = rows.load(domain, &mut table, &always)?;
        drop(fs::remove_dir_all(&dir));
        assert_eq!(report.loaded, 0);
        assert!(
            matches!(report.rejected.first(), Some(Rejected::Malformed { .. })),
            "{:?}",
            report.rejected
        );
        Ok(())
    }

    #[test]
    fn a_row_naming_an_absent_object_is_refused() -> Result<()> {
        let dir = temp_dir("dangling");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        rows.put(domain, b"request", ObjId::of(b"never stored"))?;
        let store = MemoryCas::new();
        let mut table = MemoTable::new();
        let report = rows.load(domain, &mut table, &|id| store.has(id).unwrap_or(false))?;
        drop(fs::remove_dir_all(&dir));
        assert_eq!(report.loaded, 0);
        assert!(
            matches!(report.rejected.first(), Some(Rejected::Dangling { .. })),
            "{:?}",
            report.rejected
        );
        Ok(())
    }

    #[test]
    fn a_file_that_is_not_a_key_is_refused() -> Result<()> {
        let dir = temp_dir("badname");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        let domain_dir = dir.join(domain.hash().to_hex());
        fs::create_dir_all(&domain_dir).map_err(|s| KernelError::io("mkdir", s))?;
        fs::write(domain_dir.join("README"), b"not a row")
            .map_err(|s| KernelError::io("writing", s))?;
        let mut table = MemoTable::new();
        let report = rows.load(domain, &mut table, &always)?;
        drop(fs::remove_dir_all(&dir));
        assert!(matches!(
            report.rejected.first(),
            Some(Rejected::BadName { .. })
        ));
        Ok(())
    }

    /// An interrupted write leaves a temporary behind; it is not a row and
    /// must not be reported as a broken one, or every crash would produce a
    /// permanent complaint.
    #[test]
    fn a_leftover_temporary_is_ignored_rather_than_reported() -> Result<()> {
        let dir = temp_dir("temp");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        rows.put(domain, b"request", ObjId::of(b"answer"))?;
        let domain_dir = dir.join(domain.hash().to_hex());
        fs::write(domain_dir.join(".tmp-123-4"), b"half a row")
            .map_err(|s| KernelError::io("writing", s))?;
        let mut table = MemoTable::new();
        let report = rows.load(domain, &mut table, &always)?;
        drop(fs::remove_dir_all(&dir));
        assert_eq!(report.loaded, 1);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        Ok(())
    }

    #[test]
    fn rows_are_scoped_by_domain() -> Result<()> {
        let dir = temp_dir("domains");
        let rows = DirRows::open(&dir)?;
        let one = Domain::mint("e", "one");
        let other = Domain::mint("e", "other");
        rows.put(one, b"request", ObjId::of(b"answer"))?;
        let mut table = MemoTable::new();
        let report = rows.load(other, &mut table, &always)?;
        drop(fs::remove_dir_all(&dir));
        assert_eq!(report.loaded, 0);
        assert!(table.is_empty());
        Ok(())
    }

    /// Writing the same row twice is one row, so a long-lived store does not
    /// grow a file per run.
    #[test]
    fn rewriting_a_row_does_not_add_a_second() -> Result<()> {
        let dir = temp_dir("idempotent");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        for _ in 0..3 {
            rows.put(domain, b"request", ObjId::of(b"answer"))?;
        }
        let count = fs::read_dir(dir.join(domain.hash().to_hex()))
            .map_err(|s| KernelError::io("listing", s))?
            .count();
        drop(fs::remove_dir_all(&dir));
        assert_eq!(count, 1);
        Ok(())
    }

    /// End to end against the store the evaluator actually uses: objects in a
    /// DirCas, rows in a DirRows, both reopened as a second process would.
    #[test]
    fn a_second_process_sees_the_first_process_rows() -> Result<()> {
        let dir = temp_dir("reopen");
        let domain = Domain::mint("e", "op");
        let request = b"request".to_vec();

        let output = {
            let store = DirCas::open(dir.join("objects"))?;
            let rows = DirRows::open(dir.join("index"))?;
            let output = store.put(b"the answer")?;
            rows.put(domain, &request, output)?;
            output
        };

        let store = DirCas::open(dir.join("objects"))?;
        let rows = DirRows::open(dir.join("index"))?;
        let mut table = MemoTable::new();
        let report = rows.load(domain, &mut table, &|id| store.has(id).unwrap_or(false))?;
        let found = table
            .get(domain, Key::mint(domain, &request))
            .map(|e| e.output);
        let bytes = store.get(output)?;
        drop(fs::remove_dir_all(&dir));

        assert_eq!(report.loaded, 1);
        assert_eq!(found, Some(output));
        assert_eq!(bytes, Some(b"the answer".to_vec()));
        Ok(())
    }

    #[test]
    fn a_point_lookup_finds_a_written_row() -> Result<()> {
        let dir = temp_dir("get");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        let output = ObjId::of(b"answer");
        rows.put(domain, b"request", output)?;
        let found = rows.get(domain, Key::mint(domain, b"request"));
        let absent = rows.get(domain, Key::mint(domain, b"other"));
        drop(fs::remove_dir_all(&dir));
        assert_eq!(found, Lookup::Found(output));
        assert_eq!(absent, Lookup::Missing);
        Ok(())
    }

    /// The point lookup must refuse a mis-filed row for the same reason the
    /// bulk load does, or making lookups lazy would quietly drop the check.
    #[test]
    fn a_point_lookup_refuses_a_misfiled_row() -> Result<()> {
        let dir = temp_dir("get-misfiled");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        rows.put(domain, b"request-one", ObjId::of(b"answer-one"))?;
        let domain_dir = dir.join(domain.hash().to_hex());
        let wrong = Key::mint(domain, b"request-two");
        fs::rename(
            domain_dir.join(Key::mint(domain, b"request-one").hash().to_hex()),
            domain_dir.join(wrong.hash().to_hex()),
        )
        .map_err(|s| KernelError::io("renaming", s))?;
        let found = rows.get(domain, wrong);
        drop(fs::remove_dir_all(&dir));
        assert!(
            matches!(found, Lookup::Refused(Rejected::KeyMismatch { .. })),
            "{found:?}"
        );
        Ok(())
    }

    #[test]
    fn a_point_lookup_refuses_a_truncated_row() -> Result<()> {
        let dir = temp_dir("get-truncated");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        rows.put(domain, b"request", ObjId::of(b"answer"))?;
        let key = Key::mint(domain, b"request");
        let path = dir.join(domain.hash().to_hex()).join(key.hash().to_hex());
        let bytes = fs::read(&path).map_err(|s| KernelError::io("reading", s))?;
        fs::write(&path, bytes.get(..2).unwrap_or(&[]))
            .map_err(|s| KernelError::io("writing", s))?;
        let found = rows.get(domain, key);
        drop(fs::remove_dir_all(&dir));
        assert!(
            matches!(found, Lookup::Refused(Rejected::Malformed { .. })),
            "{found:?}"
        );
        Ok(())
    }

    #[test]
    fn touching_a_row_moves_its_recorded_use() -> Result<()> {
        let dir = temp_dir("touch");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        rows.put(domain, b"request", ObjId::of(b"answer"))?;
        let key = Key::mint(domain, b"request");

        let before = rows.inventory(domain)?;
        // Set an unmistakably old time so the comparison cannot be decided by
        // filesystem timestamp granularity.
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let path = dir.join(domain.hash().to_hex()).join(key.hash().to_hex());
        let file = fs::File::open(&path).map_err(|s| KernelError::io("opening", s))?;
        file.set_modified(old)
            .map_err(|s| KernelError::io("setting mtime", s))?;
        drop(file);

        assert!(rows.touch(domain, key));
        let after = rows.inventory(domain)?;
        drop(fs::remove_dir_all(&dir));

        let used_before = before.first().map(|r| r.used);
        let used_after = after.first().map(|r| r.used);
        assert!(used_before.is_some() && used_after.is_some());
        assert!(
            used_after > Some(old),
            "touch did not move the recorded use"
        );
        Ok(())
    }

    #[test]
    fn touching_an_absent_row_is_false_rather_than_an_error() -> Result<()> {
        let dir = temp_dir("touch-absent");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        let touched = rows.touch(domain, Key::mint(domain, b"never written"));
        drop(fs::remove_dir_all(&dir));
        assert!(!touched);
        Ok(())
    }

    #[test]
    fn inventory_reports_size_and_output() -> Result<()> {
        let dir = temp_dir("inventory");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        let output = ObjId::of(b"answer");
        rows.put(domain, b"request", output)?;
        let inventory = rows.inventory(domain)?;
        drop(fs::remove_dir_all(&dir));
        assert_eq!(inventory.len(), 1);
        let row = inventory
            .first()
            .ok_or_else(|| KernelError::lock("no row"))?;
        assert_eq!(row.output, Some(output));
        assert!(row.bytes > 0);
        Ok(())
    }

    /// A row that will not parse still takes space, so it must appear in the
    /// inventory (naming no object, so a sweep reclaims it first).
    #[test]
    fn inventory_includes_rows_that_do_not_parse() -> Result<()> {
        let dir = temp_dir("inventory-bad");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        let domain_dir = dir.join(domain.hash().to_hex());
        fs::create_dir_all(&domain_dir).map_err(|s| KernelError::io("mkdir", s))?;
        fs::write(domain_dir.join("a".repeat(64)), b"not a row")
            .map_err(|s| KernelError::io("writing", s))?;
        let inventory = rows.inventory(domain)?;
        drop(fs::remove_dir_all(&dir));
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory.first().and_then(|r| r.output), None);
        Ok(())
    }

    /// A sweep that owns the store sees every domain's rows and the
    /// temporaries an interrupted write left behind, so it can credit objects
    /// to rows of domains it did not know to name and reclaim the leftovers.
    #[test]
    fn inventory_all_lists_every_domain_and_the_temporaries() -> Result<()> {
        let dir = temp_dir("inventory-all");
        let rows = DirRows::open(&dir)?;
        let one = Domain::mint("e", "one");
        let two = Domain::mint("e", "two");
        rows.put(one, b"request", ObjId::of(b"answer one"))?;
        rows.put(two, b"request", ObjId::of(b"answer two"))?;
        fs::write(
            dir.join(one.hash().to_hex()).join(".tmp-123-4"),
            b"half a row",
        )
        .map_err(|s| KernelError::io("writing", s))?;
        let all = rows.inventory_all()?;
        let per_domain = rows.inventory(one)?;
        drop(fs::remove_dir_all(&dir));
        let mut outputs: Vec<Option<ObjId>> = all.rows.iter().map(|r| r.output).collect();
        outputs.sort();
        let mut expected = vec![
            Some(ObjId::of(b"answer one")),
            Some(ObjId::of(b"answer two")),
        ];
        expected.sort();
        assert_eq!(outputs, expected);
        assert_eq!(all.temporaries.len(), 1, "{:?}", all.temporaries);
        assert!(
            matches!(all.temporaries.as_slice(), [temporary] if temporary.bytes == "half a row".len() as u64),
            "{:?}",
            all.temporaries
        );
        assert_eq!(
            per_domain.len(),
            1,
            "the per-domain inventory leaks temporaries or other domains"
        );
        Ok(())
    }

    #[test]
    fn inventory_of_an_unwritten_domain_is_empty() -> Result<()> {
        let dir = temp_dir("inventory-cold");
        let rows = DirRows::open(&dir)?;
        assert!(rows.inventory(Domain::mint("e", "op"))?.is_empty());
        drop(fs::remove_dir_all(&dir));
        Ok(())
    }

    // ---- concurrent writers ---------------------------------------------

    /// Two processes sharing one store race on the same key. The write is a
    /// rename over a fully written temporary, so a reader sees the old row or
    /// the new one and never a prefix of either.
    ///
    /// Threads rather than processes because the mechanism under test is the
    /// filesystem call sequence, which does not know the difference, and
    /// because a test that forks is a test nobody runs under a debugger.
    #[test]
    fn racing_writers_never_leave_a_torn_row() -> Result<()> {
        let dir = temp_dir("race");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        let request = b"contended".to_vec();
        let output = ObjId::of(b"answer");
        let key = Key::mint(domain, &request);
        let broken = AtomicBool::new(false);
        let wrong = AtomicBool::new(false);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let rows = &rows;
                let request = &request;
                scope.spawn(move || {
                    for _ in 0..40 {
                        drop(rows.put(domain, request, output));
                    }
                });
            }
            // Readers run throughout. Every observation must be a usable row
            // or a clean absence, never a malformed one. Recorded rather than
            // asserted in the thread: the workspace denies `panic`, and a
            // failure inside a scoped thread reports worse than one outside.
            for _ in 0..4 {
                let rows = &rows;
                let broken = &broken;
                let wrong = &wrong;
                scope.spawn(move || {
                    for _ in 0..200 {
                        match rows.get(domain, key) {
                            // A torn or half-written row would land here.
                            Lookup::Refused(_) => broken.store(true, Ordering::Relaxed),
                            Lookup::Found(found) if found != output => {
                                wrong.store(true, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        assert!(!broken.load(Ordering::Relaxed), "a reader saw a torn row");
        assert!(
            !wrong.load(Ordering::Relaxed),
            "a reader saw the wrong output"
        );

        // One row, not one per writer: the name is the key.
        let count = fs::read_dir(dir.join(domain.hash().to_hex()))
            .map_err(|s| KernelError::io("listing", s))?
            .count();
        let final_row = rows.get(domain, key);
        drop(fs::remove_dir_all(&dir));
        assert_eq!(count, 1, "racing writers left {count} files for one key");
        assert_eq!(final_row, Lookup::Found(output));
        Ok(())
    }

    /// Writers disagreeing about the answer is not a legitimate state under
    /// `Keyed` (the same request has one answer), but a corrupt or
    /// mismatched-version peer could produce it. The requirement is only that
    /// the loser is a whole row, never a mixture of both.
    #[test]
    fn racing_writers_with_different_outputs_still_leave_a_whole_row() -> Result<()> {
        let dir = temp_dir("race-disagree");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        let request = b"contended".to_vec();
        let one = ObjId::of(b"answer-one");
        let other = ObjId::of(b"answer-two");
        let key = Key::mint(domain, &request);
        let broken = AtomicBool::new(false);

        std::thread::scope(|scope| {
            for output in [one, other] {
                let rows = &rows;
                let request = &request;
                scope.spawn(move || {
                    for _ in 0..60 {
                        drop(rows.put(domain, request, output));
                    }
                });
            }
            for _ in 0..4 {
                let rows = &rows;
                let broken = &broken;
                scope.spawn(move || {
                    for _ in 0..200 {
                        if matches!(rows.get(domain, key), Lookup::Refused(_)) {
                            broken.store(true, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert!(!broken.load(Ordering::Relaxed), "a reader saw a torn row");

        let settled = rows.get(domain, key);
        drop(fs::remove_dir_all(&dir));
        // Exactly one of the two, whole. Never a third thing.
        assert!(
            settled == Lookup::Found(one) || settled == Lookup::Found(other),
            "settled on {settled:?}"
        );
        Ok(())
    }

    /// The guard above is only meaningful if a non-atomic write would trip it.
    /// This writes a row the way `put` deliberately does not: straight to the
    /// final path, so a reader can catch it half-written.
    ///
    /// It asserts that the torn state is *observable*, which is what makes
    /// `racing_writers_never_leave_a_torn_row` a test of rename-into-place
    /// rather than a test of the filesystem being fast.
    #[test]
    fn a_non_atomic_write_is_observably_torn() -> Result<()> {
        use std::io::Write as _;

        let dir = temp_dir("torn");
        let rows = DirRows::open(&dir)?;
        let domain = Domain::mint("e", "op");
        // A large request, so the write is many syscalls wide and a reader has
        // somewhere to land in the middle of it.
        let request = vec![b'x'; 512 * 1024];
        let key = Key::mint(domain, &request);
        let path = dir.join(domain.hash().to_hex()).join(key.hash().to_hex());
        fs::create_dir_all(dir.join(domain.hash().to_hex()))
            .map_err(|s| KernelError::io("mkdir", s))?;
        let bytes = encode_row(&request, ObjId::of(b"answer"))?;

        let torn = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let path = &path;
            let bytes = &bytes;
            scope.spawn(move || {
                for _ in 0..20 {
                    // The bad pattern: truncate in place, then fill.
                    if let Ok(mut file) = fs::File::create(path) {
                        for chunk in bytes.chunks(4096) {
                            if file.write_all(chunk).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            let torn = &torn;
            let rows = &rows;
            scope.spawn(move || {
                for _ in 0..4000 {
                    if matches!(rows.get(domain, key), Lookup::Refused(_)) {
                        torn.store(true, Ordering::Relaxed);
                    }
                }
            });
        });

        let observed = torn.load(Ordering::Relaxed);
        drop(fs::remove_dir_all(&dir));
        assert!(
            observed,
            "a non-atomic write was never caught mid-flight, so the atomic \
             writer's clean record proves nothing; make the payload larger"
        );
        Ok(())
    }
}
