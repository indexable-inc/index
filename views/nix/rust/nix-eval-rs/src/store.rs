//! The on-disk evaluation cache as one thing, and the sweep that bounds it.
//!
//! Three directories under one root: `objects/` (content-addressed module and
//! result bytes), `index/` (memo rows), `witness/` (the questions each
//! module's evaluation asked). They are separate because they are addressed
//! differently, and they are here together because nothing else knows how all
//! three relate.
//!
//! # Why an unbounded store is not acceptable
//!
//! Measured over an edit loop: a tree of 22 files with one file edited 50
//! times grows by 495 bytes, 3 rows and 1 witness per edit, while the working
//! set stays flat at 22 entries. After 50 edits, 194 rows exist and 22 are
//! ever consulted; the other 89% are versions nobody will ask for again.
//! Growth tracks source size at roughly one copy per edit (a 200 KiB file
//! costs 200 KiB per edit), because a row carries the canonical request and
//! the request carries the source. Editing one large file in a loop is
//! therefore the case that hurts, and it is the ordinary case.
//!
//! # Recency of use, not of write
//!
//! The entries worth keeping under an edit loop are written once and then only
//! read; the churn is freshly written every round. Evicting by write time
//! removes exactly the working set and keeps exactly the garbage, so a hit
//! marks its row and its witness used ([`DirRows::touch`],
//! [`crate::readset::DirWitness::touch`]: metadata-only updates) and the
//! sweep orders by that.
//!
//! # The sweep reads metadata, never a witness
//!
//! A witness for the home target is 400 MB; the sweep learns what it names
//! from a sidecar the writer puts beside it (`witness/<id>.refs`), so a
//! sweep over a multi-gigabyte cache is a directory listing and a few
//! hundred small reads. See [`Store::sweep`].
//!
//! # The under-cap check is a listing, not a stat per object
//!
//! Objects are content-addressed and never rewritten, so an object's size is
//! a fact about its name. `objects.sizes` under the root remembers them; the
//! check that runs after every publication lists `objects/` and stats only
//! the names the memo lacks. Measured before this memo (hydra, 2026-09-04):
//! 41,120 `fstatat` and 1.3 s of system time per publication over 37,554
//! objects, on a `nix eval` whose evaluation took 0.05 s. See
//! [`Store::sweep_to_cap`] for what the memo may and may not decide.
//!
//! # Eviction can only cause a miss
//!
//! Everything here is [`Policy::Keyed`], so removing any completed entry is
//! always safe: the effect re-performs. The sweep is best-effort and runs
//! after the answers have already been given. It shares one short-lived file
//! lock with cache publication so it never mistakes half a completed entry
//! for garbage. The gate's assertion remains: after a sweep, every answer is
//! still byte-identical to a fresh process.
//!
//! [`Policy::Keyed`]: ix_kernel::Policy::Keyed
//! [`DirRows::touch`]: ix_kernel::DirRows::touch

use ix_kernel::cas::DirCas;
use ix_kernel::rows::{DirRows, RowInfo};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

const PUBLICATION_LOCK: &str = ".record-sweep.lock";
/// Remembered object sizes, `<root>/objects.sizes`: a header line, then
/// `<object name>\t<bytes>` per object. Not counted by the census (a few
/// MB for the largest cache seen) and never swept. Written whole then renamed
/// into place, under the publication lock.
const OBJECT_SIZES: &str = "objects.sizes";
const OBJECT_SIZES_HEADER: &str = "ixe-object-sizes 1";

/// How a census learns each object's size.
/// One read of the objects directory: every object by name with its size,
/// and the `.tmp-*` temporaries beside them (a writer's half-finished
/// object, swept as a leftover).
struct ObjectListing {
    objects: BTreeMap<String, u64>,
    temporaries: Vec<(PathBuf, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObjectSizes {
    /// From `objects.sizes` where it names the object, a stat where it does
    /// not. Decides only whether to look closer: the one way it can be wrong
    /// is an object repaired by `put` between two measured censuses, whose
    /// remembered size is the damaged one.
    Remembered,
    /// A stat per object, and the memo rewritten from them. Every census a
    /// removal acts on, so what is removed and what it frees rest on real
    /// sizes.
    Measured,
}

/// The store-wide exclusion between publishing cache entries and sweeping.
///
/// The operating system releases the lock if a process exits while holding
/// it, so an interrupted evaluator cannot leave a stale lock behind.
pub(crate) struct PublicationGuard {
    file: File,
}

impl Drop for PublicationGuard {
    fn drop(&mut self) {
        drop(File::unlock(&self.file));
    }
}

fn publication_guard(root: &Path, blocking: bool) -> std::io::Result<PublicationGuard> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(PUBLICATION_LOCK))?;
    if blocking {
        File::lock(&file)?;
    } else {
        File::try_lock(&file)?;
    }
    Ok(PublicationGuard { file })
}

/// What a sweep removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Witnesses without a readable sidecar, all of which were removed.
    ///
    /// Reported apart from `witnesses_removed` because the two mean different
    /// things and the difference is the ENG-12601 signature: reclaiming a
    /// witness whose objects are genuinely gone is the sweep working, and
    /// finding every witness unreadable is the sweep destroying a cache it
    /// merely failed to understand. A caller that watches only the total
    /// cannot tell those apart, and the total looked healthy while the cache
    /// served nothing.
    pub witnesses_unreadable: usize,
    /// Witnesses still present when the sweep finished, so a caller can see
    /// "removed 5, 0 left" -- which is the shape worth shouting about -- as
    /// distinct from "removed 5, 40 left".
    pub witnesses_left: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub rows_removed: usize,
    pub objects_removed: usize,
    pub witnesses_removed: usize,
    /// `.tmp-*` files of interrupted writes and `.refs` sidecars whose
    /// witness is gone. Never live, so removed cap or no cap.
    pub leftovers_removed: usize,
}

/// The evaluation cache's directory layout.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
    cas: DirCas,
    rows: DirRows,
    witness: crate::readset::DirWitness,
    sealed: crate::readset::DirSealed,
    copies: crate::readset::DirCopies,
    /// Bytes the store is swept down to after a publication; 0 never sweeps.
    max_bytes: u64,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        for part in ["objects", "index", "witness", "sealed", "copies"] {
            std::fs::create_dir_all(root.join(part))?;
        }
        let cas = DirCas::open(root.join("objects")).map_err(std::io::Error::other)?;
        let rows = DirRows::open(root.join("index")).map_err(std::io::Error::other)?;
        let witness = crate::readset::DirWitness::open(root.join("witness"))?;
        let sealed = crate::readset::DirSealed::open(root.join("sealed"))?;
        let copies = crate::readset::DirCopies::open(root.join("copies"))?;
        Ok(Self {
            root,
            cas,
            rows,
            witness,
            sealed,
            copies,
            max_bytes: 0,
        })
    }

    /// Cap the store at `max_bytes`; 0 leaves it unbounded. Enforced by
    /// [`Store::sweep_to_cap`] after each result publication.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// The store objects recorded as sealed. Not swept: a fact about
    /// immutable bytes, one file per object (`sealed/<hash>-<name>`, body =
    /// a digest line then the symlinks leaving the object), a few hundred in
    /// the largest cache seen.
    #[must_use]
    pub fn sealed(&self) -> &crate::readset::DirSealed {
        &self.sealed
    }

    /// The per-question memo of filtered copies: `copies/<key>`, body = a
    /// store path, pruned by its own count ([`crate::readset::DirCopies`]),
    /// not by this store's byte cap, and served only through
    /// [`crate::readset::CopyMemo`], which decides when an entry is sound.
    #[must_use]
    pub fn copy_memo(&self, store_dir: Option<String>) -> crate::readset::CopyMemo {
        crate::readset::CopyMemo::new(self.copies.clone(), self.sealed.clone(), store_dir)
    }

    #[must_use]
    pub fn cas(&self) -> &DirCas {
        &self.cas
    }

    #[must_use]
    pub fn rows(&self) -> &DirRows {
        &self.rows
    }

    #[must_use]
    pub fn witness(&self) -> &crate::readset::DirWitness {
        &self.witness
    }

    #[must_use]
    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    #[must_use]
    pub fn index_dir(&self) -> PathBuf {
        self.root.join("index")
    }

    #[must_use]
    pub fn witness_dir(&self) -> PathBuf {
        self.root.join("witness")
    }

    pub(crate) fn publication_guard(&self) -> std::io::Result<PublicationGuard> {
        publication_guard(&self.root, true)
    }

    #[cfg(test)]
    pub(crate) fn try_publication_guard(&self) -> std::io::Result<PublicationGuard> {
        publication_guard(&self.root, false)
    }

    /// Bytes in the swept directories (`objects`, `index`, `witness`),
    /// temporaries included, object sizes as [`ObjectSizes::Remembered`].
    /// `sealed/` is a permanent record of a few hundred small files and is
    /// neither counted nor swept, nor is `objects.sizes`. This is the census
    /// the under-cap check reads, so a directory that will not list is an
    /// error and not a smaller number: a store reporting itself under cap
    /// because half of it could not be read is the failure that looks like
    /// success. Takes the publication lock, because the census may write the
    /// memo.
    pub fn size(&self) -> std::io::Result<u64> {
        let _publication = self.publication_guard()?;
        Ok(self.read_census(ObjectSizes::Remembered)?.total)
    }

    /// Bytes this store is swept down to after a publication, or 0 for
    /// never. See [`Store::sweep_to_cap`].
    #[must_use]
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Bring the store under its cap if it has one and is over it. `Ok(None)`
    /// when nothing needed doing.
    ///
    /// Called after a publication ([`crate::readset::ResultCache::record`],
    /// and once at the end of [`crate::session::evaluate_value_once`], which
    /// publishes modules but never a result), so the answer has already been
    /// given and the only thing a sweep can cost is a later miss. The
    /// decision is taken from a census under the publication lock, the same
    /// census the sweep then acts on: a size read outside the lock can be
    /// undercut by a publication in flight, and one that failed open (a
    /// directory that would not list counted as empty) is a cap nobody is
    /// enforcing. Paid once per publication and never on a hit; `sweep.ns`
    /// in the perf line is the measured cost, whether or not anything was
    /// removed.
    ///
    /// Two censuses when the store looks full, one when it does not. The
    /// first takes object sizes from the memo ([`ObjectSizes::Remembered`]):
    /// a listing plus a stat per object the memo has not seen, which is what
    /// a publication under the cap costs. Only when that says over the cap is
    /// every object measured, and the removal acts on the measured census:
    /// the memo decides "look closer", never what goes.
    pub fn sweep_to_cap(&self) -> std::io::Result<Option<SweepReport>> {
        if self.max_bytes == 0 {
            return Ok(None);
        }
        let started = std::time::Instant::now();
        let outcome = self.publication_guard().and_then(|_publication| {
            if self.read_census(ObjectSizes::Remembered)?.total <= self.max_bytes {
                return Ok(None);
            }
            let census = self.read_census(ObjectSizes::Measured)?;
            if census.total <= self.max_bytes {
                return Ok(None);
            }
            self.remove(&census, self.max_bytes).map(Some)
        });
        note_sweep(started, outcome.as_ref().map(Option::as_ref));
        outcome
    }

    /// Remove least-recently-used entries until the store fits in `max_bytes`,
    /// then every object nothing left names.
    ///
    /// # Metadata only
    ///
    /// The sweep never reads a witness body. A home-target witness is 400 MB
    /// and a cache holds hundreds of them, so a sweep that parsed witnesses to
    /// learn which ATerms they name read 2.7 GB before deleting anything
    /// (hydra, 2026-09-03). The writer puts what the sweep needs beside the
    /// witness instead: `witness/<id>.refs`, the object names its rows refer
    /// to, one per line ([`crate::readset::DirWitness::put`]). The sweep's
    /// inputs are directory listings, row files (a few hundred bytes each) and
    /// those sidecars, so its cost is the number of entries, not their size.
    ///
    /// # Order
    ///
    /// Rows and witnesses form one LRU: oldest use first, ties by name so a
    /// sweep is deterministic given the same directory. A hit touches both
    /// its row ([`DirRows::touch`]) and its witness
    /// ([`crate::readset::DirWitness::touch`]); an entry never touched carries
    /// its write time. Removing an entry frees its file plus every object it
    /// was the last to name, which is what stops the loop evicting every row
    /// before the object pass reclaims anything. Objects are then swept by
    /// reference count, cap or no cap: an object no row output and no sidecar
    /// names can never be found again. A row and its witness are touched
    /// together and written together, so they age together; the cap boundary
    /// can still fall between them, and the survivor is then a miss that the
    /// next record rewrites and the next sweep's LRU tail.
    ///
    /// # What is dead regardless of the cap
    ///
    /// A witness without a readable sidecar, and a witness whose sidecar names
    /// an object the store no longer holds. Neither can ever produce a hit
    /// (the first is a format this build did not write, the second fails
    /// replay on a missing ATerm), so keeping them costs bytes and buys
    /// nothing. The first is counted in `witnesses_unreadable` as well as
    /// `witnesses_removed`, because a run where every witness is unreadable
    /// is a broken format and not a tidy store (ENG-12601). Temporaries and
    /// sidecars without their witness are leftovers of interrupted writes and
    /// removals: nothing is mid-write under the publication lock, so they go
    /// too, and they are counted so the cap arithmetic includes them.
    ///
    /// # Every domain
    ///
    /// Rows of every domain under `index/` are one LRU, and every one of them
    /// roots the object it names. The first version swept only the domains it
    /// was told about and counted references only from those, so the objects
    /// behind another domain's rows were reclaimed while the rows stayed,
    /// pointing at nothing.
    ///
    /// # Census before deletion, and no deletion on a partial census
    ///
    /// Every set this sweep keeps alive is computed from reads that can fail:
    /// a row directory, a row, a sidecar. A failed read that quietly yields an
    /// empty set does not make the sweep conservative, it makes it delete
    /// everything the unread part referenced -- live witnesses and the ATerms
    /// behind them -- and report a healthy store. So the whole census is taken
    /// first and any failure aborts before a single file is removed. The one
    /// tolerated failure is a file gone between listing and reading: a lookup
    /// refusing a corrupt witness removes it without the lock, and what is
    /// gone keeps nothing alive. A removal that fails is an error too: the
    /// store is left consistent (rows, witnesses and objects are independent
    /// files, and a sidecar is removed after its witness), and the next sweep
    /// finishes the job.
    pub fn sweep(&self, max_bytes: u64) -> std::io::Result<SweepReport> {
        let started = std::time::Instant::now();
        let outcome = self.publication_guard().and_then(|_publication| {
            let census = self.read_census(ObjectSizes::Measured)?;
            self.remove(&census, max_bytes)
        });
        note_sweep(started, outcome.as_ref().map(Some));
        outcome
    }

    /// Everything in the swept directories, classified as the sweep would
    /// classify it, read under the publication lock so nothing is mid-write.
    /// For reporting; [`Store::sweep`] takes its own.
    pub fn census(&self) -> std::io::Result<Census> {
        let _publication = self.publication_guard()?;
        self.read_census(ObjectSizes::Measured)
    }

    /// Under the publication lock: the object listing may rewrite the memo.
    fn read_census(&self, sizes: ObjectSizes) -> std::io::Result<Census> {
        let ix_kernel::rows::Inventory { rows, temporaries } = self
            .rows
            .inventory_all()
            .map_err(|e| std::io::Error::other(format!("listing rows: {e}")))?;
        let mut leftovers: Vec<(PathBuf, u64)> = temporaries
            .into_iter()
            .map(|temporary| (temporary.path, temporary.bytes))
            .collect();

        let ObjectListing {
            objects,
            temporaries,
        } = self.object_listing(sizes)?;
        leftovers.extend(temporaries);

        let listed = entries(&self.witness_dir())?;
        leftovers.extend(listed.temporaries);
        let mut sidecars: BTreeMap<String, Entry> = BTreeMap::new();
        let mut bodies: Vec<Entry> = Vec::new();
        for entry in listed.files {
            match entry.name.strip_suffix(crate::readset::REFS_SUFFIX) {
                Some(owner) => {
                    sidecars.insert(owner.to_owned(), entry);
                }
                None => bodies.push(entry),
            }
        }
        let mut witnesses = Vec::with_capacity(bodies.len());
        for body in bodies {
            let sidecar = sidecars.remove(&body.name);
            let refs = match &sidecar {
                Some(sidecar) => match std::fs::read_to_string(&sidecar.path) {
                    Ok(text) => crate::readset::witness_refs(&text),
                    // Not UTF-8, or gone since the listing: no readable
                    // sidecar, which makes the witness dead, not the sweep
                    // impossible.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::InvalidData | std::io::ErrorKind::NotFound
                        ) =>
                    {
                        None
                    }
                    Err(error) => return Err(error),
                },
                None => None,
            };
            witnesses.push(WitnessInfo {
                name: body.name,
                path: body.path,
                bytes: body.bytes + sidecar.as_ref().map_or(0, |s| s.bytes),
                used: body.modified,
                sidecar: sidecar.map(|s| s.path),
                refs,
            });
        }
        leftovers.extend(
            sidecars
                .into_values()
                .map(|entry| (entry.path, entry.bytes)),
        );

        let total = rows.iter().map(|row| row.bytes).sum::<u64>()
            + witnesses.iter().map(|w| w.bytes).sum::<u64>()
            + leftovers.iter().map(|(_, bytes)| bytes).sum::<u64>()
            + objects.values().sum::<u64>();
        Ok(Census {
            total,
            rows,
            witnesses,
            leftovers,
            objects,
        })
    }

    /// Every object by name and size, and the `.tmp-*` temporaries beside
    /// them. Sizes come from the memo or a stat as `how` says; the memo is
    /// rewritten when it did not describe the directory (an object it lacked
    /// or one that is gone), or always when measured. Objects gone between
    /// listing and stat were removed by someone else and are not part of the
    /// store. Under the publication lock.
    fn object_listing(&self, how: ObjectSizes) -> std::io::Result<ObjectListing> {
        let remembered = match how {
            ObjectSizes::Remembered => self.read_object_sizes(),
            ObjectSizes::Measured => BTreeMap::new(),
        };
        let mut objects: BTreeMap<String, u64> = BTreeMap::new();
        let mut temporaries = Vec::new();
        let read = match std::fs::read_dir(self.objects_dir()) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ObjectListing {
                    objects,
                    temporaries,
                });
            }
            Err(error) => return Err(error),
        };
        for entry in read {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let temporary = name.starts_with(".tmp-");
            if !temporary && let Some(bytes) = remembered.get(&name) {
                objects.insert(name, *bytes);
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !metadata.is_file() {
                continue;
            }
            if temporary {
                temporaries.push((entry.path(), metadata.len()));
            } else {
                objects.insert(name, metadata.len());
            }
        }
        let described = objects.len() == remembered.len()
            && objects.keys().all(|name| remembered.contains_key(name));
        if how == ObjectSizes::Measured || !described {
            self.write_object_sizes(&objects)?;
        }
        Ok(ObjectListing {
            objects,
            temporaries,
        })
    }

    /// The memo's contents, or nothing when there is no memo or it is not one
    /// this build wrote: a line that does not parse makes the whole memo
    /// unread, so every object is measured and the memo rewritten, rather
    /// than the readable half trusted.
    fn read_object_sizes(&self) -> BTreeMap<String, u64> {
        let Ok(text) = std::fs::read_to_string(self.root.join(OBJECT_SIZES)) else {
            return BTreeMap::new();
        };
        let mut lines = text.lines();
        if lines.next() != Some(OBJECT_SIZES_HEADER) {
            return BTreeMap::new();
        }
        let mut sizes = BTreeMap::new();
        for line in lines {
            let Some((name, bytes)) = line.split_once('\t') else {
                return BTreeMap::new();
            };
            let Ok(bytes) = bytes.parse::<u64>() else {
                return BTreeMap::new();
            };
            sizes.insert(name.to_owned(), bytes);
        }
        sizes
    }

    /// Write the memo whole, then rename it into place. The pid suffices for
    /// the temporary: writers hold the publication lock.
    fn write_object_sizes(&self, objects: &BTreeMap<String, u64>) -> std::io::Result<()> {
        let mut text = String::with_capacity(OBJECT_SIZES_HEADER.len() + 1 + objects.len() * 80);
        text.push_str(OBJECT_SIZES_HEADER);
        text.push('\n');
        for (name, bytes) in objects {
            text.push_str(name);
            text.push('\t');
            text.push_str(&bytes.to_string());
            text.push('\n');
        }
        let temporary = self
            .root
            .join(format!(".{OBJECT_SIZES}.{}", std::process::id()));
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, self.root.join(OBJECT_SIZES))
    }

    /// Act on a census: the removal half of [`Store::sweep`].
    fn remove(&self, census: &Census, max_bytes: u64) -> std::io::Result<SweepReport> {
        let mut report = SweepReport {
            bytes_before: census.total,
            ..SweepReport::default()
        };
        let mut total = census.total;

        for (path, bytes) in &census.leftovers {
            remove_if_present(path)?;
            total = total.saturating_sub(*bytes);
            report.leftovers_removed += 1;
        }

        // Dead witnesses first, so the reference counts below describe exactly
        // the witnesses that will survive this sweep.
        let Liveness {
            dead,
            live,
            mut refcount,
        } = census.liveness();
        for (witness, why) in dead {
            if why == DeadWitness::Unreadable {
                report.witnesses_unreadable += 1;
            }
            total = total.saturating_sub(witness.remove()?);
            report.witnesses_removed += 1;
        }

        // One LRU over rows and witnesses.
        enum Which<'a> {
            Row(&'a RowInfo),
            Witness(&'a WitnessInfo),
        }
        let mut victims: Vec<(std::time::SystemTime, &str, Which<'_>)> = census
            .rows
            .iter()
            .map(|row| (row.used, row.name.as_str(), Which::Row(row)))
            .chain(
                live.iter()
                    .map(|witness| (witness.used, witness.name.as_str(), Which::Witness(witness))),
            )
            .collect();
        victims.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));

        // Objects whose bytes the loop already credited, so the object pass
        // does not credit them twice.
        let mut credited: BTreeSet<String> = BTreeSet::new();
        let mut witnesses_left = live.len();
        for (_, _, which) in victims {
            if total <= max_bytes {
                break;
            }
            let named: Vec<String> = match which {
                Which::Row(row) => {
                    remove_if_present(&row.path)?;
                    total = total.saturating_sub(row.bytes);
                    report.rows_removed += 1;
                    row.output
                        .iter()
                        .map(|output| output.hash().to_hex())
                        .collect()
                }
                Which::Witness(witness) => {
                    total = total.saturating_sub(witness.remove()?);
                    report.witnesses_removed += 1;
                    witnesses_left -= 1;
                    witness.names().map(str::to_owned).collect()
                }
            };
            for name in named {
                let remaining = refcount.get_mut(&name).ok_or_else(|| {
                    std::io::Error::other(format!(
                        "sweep: object {name} is named by a removed entry but was never counted"
                    ))
                })?;
                *remaining = remaining.saturating_sub(1);
                if *remaining == 0 && credited.insert(name.clone()) {
                    total = total.saturating_sub(census.objects.get(&name).copied().unwrap_or(0));
                }
            }
        }

        // The object pass: whatever nothing names any more.
        for (name, bytes) in &census.objects {
            if refcount.get(name).copied().unwrap_or(0) != 0 {
                continue;
            }
            remove_if_present(&self.objects_dir().join(name))?;
            report.objects_removed += 1;
            if !credited.contains(name) {
                total = total.saturating_sub(*bytes);
            }
        }

        report.witnesses_left = witnesses_left;
        report.bytes_after = total;
        Ok(report)
    }
}

/// Everything in the swept directories, read before anything is removed, and
/// the one place the liveness rule lives: the sweep acts on it and
/// `eval-server --scrub` reports from it, so the two cannot drift (ENG-12884
/// was a scrub applying a rule the sweep had stopped applying, calling every
/// witness orphaned at exit 0).
#[derive(Debug)]
pub struct Census {
    total: u64,
    rows: Vec<RowInfo>,
    witnesses: Vec<WitnessInfo>,
    /// Never live: temporaries of interrupted writes, and `.refs` sidecars
    /// whose witness is gone.
    leftovers: Vec<(PathBuf, u64)>,
    /// Object name to size.
    objects: BTreeMap<String, u64>,
}

impl Census {
    /// Bytes across rows, witnesses, sidecars, leftovers and objects.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn witnesses(&self) -> usize {
        self.witnesses.len()
    }

    #[must_use]
    pub fn objects(&self) -> usize {
        self.objects.len()
    }

    #[must_use]
    pub fn leftovers(&self) -> usize {
        self.leftovers.len()
    }

    /// Witnesses a sweep removes cap or no cap, by name, with why.
    #[must_use]
    pub fn dead_witnesses(&self) -> Vec<(String, DeadWitness)> {
        self.liveness()
            .dead
            .into_iter()
            .map(|(witness, why)| (witness.name.clone(), why))
            .collect()
    }

    /// Objects named by no row output and no live witness: what the object
    /// pass of a sweep removes.
    #[must_use]
    pub fn unreferenced_objects(&self) -> Vec<String> {
        let referenced = self.liveness().refcount;
        self.objects
            .keys()
            .filter(|name| !referenced.contains_key(*name))
            .cloned()
            .collect()
    }

    fn liveness(&self) -> Liveness<'_> {
        let mut dead = Vec::new();
        let mut live = Vec::new();
        for witness in &self.witnesses {
            match witness.why_dead(&self.objects) {
                Some(why) => dead.push((witness, why)),
                None => live.push(witness),
            }
        }
        let mut refcount: BTreeMap<String, usize> = BTreeMap::new();
        for name in self
            .rows
            .iter()
            .filter_map(|row| row.output)
            .map(|output| output.hash().to_hex())
        {
            *refcount.entry(name).or_default() += 1;
        }
        for name in live.iter().flat_map(|witness| witness.names()) {
            *refcount.entry(name.to_owned()).or_default() += 1;
        }
        Liveness {
            dead,
            live,
            refcount,
        }
    }
}

/// The liveness rule applied to one census.
struct Liveness<'a> {
    dead: Vec<(&'a WitnessInfo, DeadWitness)>,
    live: Vec<&'a WitnessInfo>,
    /// How many row outputs and live sidecars name each object.
    refcount: BTreeMap<String, usize>,
}

/// Why a witness can never produce a hit again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeadWitness {
    /// No sidecar, or one that is not a list of object names: a format this
    /// build did not write.
    Unreadable,
    /// The sidecar names an object the store no longer holds, so replay fails
    /// on the missing ATerm every time.
    MissingObject(String),
}

impl core::fmt::Display for DeadWitness {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unreadable => f.write_str("has no readable sidecar naming what it keeps alive"),
            Self::MissingObject(name) => write!(f, "names object {name}, which is gone"),
        }
    }
}

/// One witness as the sweep sees it: its files and their metadata, never
/// its body.
#[derive(Debug)]
struct WitnessInfo {
    name: String,
    path: PathBuf,
    /// Witness plus sidecar.
    bytes: u64,
    /// Last use: the witness file's mtime, moved by
    /// [`crate::readset::DirWitness::touch`].
    used: std::time::SystemTime,
    sidecar: Option<PathBuf>,
    /// The object names the sidecar lists, or `None` when there is no
    /// readable sidecar.
    refs: Option<Vec<String>>,
}

impl WitnessInfo {
    fn names(&self) -> impl Iterator<Item = &str> {
        self.refs.iter().flatten().map(String::as_str)
    }

    /// `None` when every object the sidecar names is present.
    fn why_dead(&self, objects: &BTreeMap<String, u64>) -> Option<DeadWitness> {
        let Some(refs) = &self.refs else {
            return Some(DeadWitness::Unreadable);
        };
        refs.iter()
            .find(|name| !objects.contains_key(*name))
            .map(|name| DeadWitness::MissingObject(name.clone()))
    }

    /// Remove the witness, then its sidecar; the bytes freed.
    fn remove(&self) -> std::io::Result<u64> {
        remove_if_present(&self.path)?;
        if let Some(sidecar) = &self.sidecar {
            remove_if_present(sidecar)?;
        }
        Ok(self.bytes)
    }
}

/// One regular file of a store directory, as its metadata describes it.
#[derive(Debug)]
struct Entry {
    path: PathBuf,
    name: String,
    bytes: u64,
    modified: std::time::SystemTime,
}

/// A directory listing split into entries and `.tmp-*` temporaries.
struct Listing {
    files: Vec<Entry>,
    temporaries: Vec<(PathBuf, u64)>,
}

/// The regular files directly in `dir`. A directory that does not exist yet
/// is empty; any other failure is the caller's to refuse on, because a
/// partial listing read as a complete one is how a sweep deletes what the
/// unlisted part referenced. An entry gone between listing and stat was
/// removed by someone else and is not part of the store any more.
fn entries(dir: &Path) -> std::io::Result<Listing> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Listing {
                files: Vec::new(),
                temporaries: Vec::new(),
            });
        }
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    let mut temporaries = Vec::new();
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() {
            continue;
        }
        if name.starts_with(".tmp-") {
            temporaries.push((entry.path(), metadata.len()));
            continue;
        }
        files.push(Entry {
            path: entry.path(),
            name,
            bytes: metadata.len(),
            modified: metadata.modified()?,
        });
    }
    Ok(Listing { files, temporaries })
}

/// Remove a file the census listed. One already gone was removed by a lookup
/// refusing a corrupt witness, which does not hold the publication lock;
/// either way the bytes are gone, which is what the sweep wanted.
fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Account one sweep attempt, removed or not, failed or not, so the perf
/// line shows what the cap costs and how often enforcing it failed.
fn note_sweep(started: std::time::Instant, outcome: Result<Option<&SweepReport>, &std::io::Error>) {
    let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    match outcome {
        Ok(Some(report)) => crate::perf::note_sweep(
            nanos,
            u64::try_from(
                report.rows_removed
                    + report.witnesses_removed
                    + report.objects_removed
                    + report.leftovers_removed,
            )
            .unwrap_or(u64::MAX),
            report.bytes_before.saturating_sub(report.bytes_after),
        ),
        Ok(None) => crate::perf::note_sweep(nanos, 0, 0),
        Err(_) => crate::perf::note_sweep_failed(nanos),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ix_kernel::cas::{Cas, DirCas};
    use ix_kernel::{Domain, Key};

    fn scratch(label: &str) -> PathBuf {
        crate::eval::scratch_dir("ixe-store", label)
    }

    fn domain() -> Domain {
        Domain::mint("test.effect", "op")
    }

    /// Write `n` entries, each an object plus a row naming it.
    fn fill(store: &Store, n: usize) -> std::io::Result<Vec<Key>> {
        let cas = DirCas::open(store.objects_dir()).map_err(std::io::Error::other)?;
        let rows = DirRows::open(store.index_dir()).map_err(std::io::Error::other)?;
        let mut keys = Vec::new();
        for i in 0..n {
            let request = format!("request-{i}").into_bytes();
            // Distinct per entry: identical payloads are one object under
            // content addressing, and a single object shared by every row
            // cannot be freed until the last row goes, which is a different
            // situation from the one these tests mean to set up.
            let mut payload = vec![b'p'; 1024];
            payload.extend_from_slice(format!("-{i}").as_bytes());
            let output = cas.put(&payload).map_err(std::io::Error::other)?;
            rows.put(domain(), &request, output)
                .map_err(std::io::Error::other)?;
            keys.push(Key::mint(domain(), &request));
        }
        Ok(keys)
    }

    #[test]
    fn a_store_under_its_cap_is_left_alone() -> std::io::Result<()> {
        let dir = scratch("under");
        let store = Store::open(&dir)?;
        fill(&store, 4)?;
        let before = store.size()?;
        let report = store.sweep(u64::MAX)?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(report.rows_removed, 0);
        assert_eq!(report.bytes_after, before);
        Ok(())
    }

    #[test]
    fn sweeping_brings_a_store_under_its_cap() -> std::io::Result<()> {
        let dir = scratch("cap");
        let store = Store::open(&dir)?;
        fill(&store, 20)?;
        let cap = store.size()? / 2;
        let report = store.sweep(cap)?;
        let after = store.size()?;
        drop(std::fs::remove_dir_all(&dir));
        assert!(report.rows_removed > 0, "{report:?}");
        assert!(after <= cap, "{after} bytes with a cap of {cap}");
        Ok(())
    }

    /// The property the whole policy exists for. Under an edit loop the
    /// entries worth keeping are the ones being read, not the ones being
    /// written, so a sweep must keep a touched row and drop an untouched one
    /// even though the untouched one was written later.
    #[test]
    fn a_used_row_outlives_a_newer_unused_one() -> std::io::Result<()> {
        let dir = scratch("recency");
        let store = Store::open(&dir)?;
        let rows = DirRows::open(store.index_dir()).map_err(std::io::Error::other)?;
        let keys = fill(&store, 12)?;

        // Age everything, then mark only the first few as used now. Without
        // the explicit ageing this depends on filesystem timestamp
        // granularity, which is coarse enough to make the test lie.
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for key in &keys {
            let path = store
                .index_dir()
                .join(domain().hash().to_hex())
                .join(key.hash().to_hex());
            if let Ok(file) = std::fs::File::open(&path) {
                drop(file.set_modified(old));
            }
        }
        let kept: Vec<Key> = keys.iter().take(3).copied().collect();
        for key in &kept {
            assert!(rows.touch(domain(), *key));
        }

        store.sweep(store.size()? / 2)?;
        let survived: Vec<bool> = kept
            .iter()
            .map(|key| matches!(rows.get(domain(), *key), ix_kernel::Lookup::Found(_)))
            .collect();
        drop(std::fs::remove_dir_all(&dir));
        assert!(
            survived.iter().all(|&s| s),
            "a touched row was evicted while untouched ones remained: {survived:?}"
        );
        Ok(())
    }

    #[test]
    fn objects_no_row_names_are_reclaimed() -> std::io::Result<()> {
        let dir = scratch("orphans");
        let store = Store::open(&dir)?;
        let cas = DirCas::open(store.objects_dir()).map_err(std::io::Error::other)?;
        fill(&store, 3)?;
        // An object nothing points at.
        let orphan = cas
            .put(b"nobody references this")
            .map_err(std::io::Error::other)?;
        assert!(cas.has(orphan).map_err(std::io::Error::other)?);

        let report = store.sweep(u64::MAX)?;
        let still_there = cas.has(orphan).map_err(std::io::Error::other)?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(report.objects_removed, 1, "{report:?}");
        assert!(!still_there);
        Ok(())
    }

    /// The question every identity in these tests is filed under. Which one
    /// does not matter here -- the sweep never reads a witness body, so the
    /// question inside is invisible to it -- so one spelling keeps the tests
    /// about the sweep.
    fn whole_question() -> crate::session::Question {
        crate::session::Question::Whole {
            render: crate::session::RenderMode::Plain,
        }
    }

    /// An evaluation identity distinguished by `label`.
    fn identity(label: &[u8]) -> crate::readset::EvalId {
        crate::readset::EvalId::of(
            &ix_kernel::hash::tagged("test-module", &[label]),
            &crate::eval::Settings::default(),
            &crate::session::Arguments::none(),
            &whole_question(),
        )
    }

    /// A witness whose only row writes the derivation `aterm`, so its sidecar
    /// names exactly that object.
    fn derivation_row(aterm: ix_kernel::ObjId) -> crate::readset::WitnessRow {
        (
            crate::readset::Question::WriteDrv {
                name: "d".to_owned(),
                answer: crate::readset::WriteDrvAnswer::Written("/nix/store/d.drv".to_owned()),
                aterm,
            },
            crate::readset::Recorded::Digest(ix_kernel::Hash::from_bytes([0; 32])),
        )
    }

    fn witness_files(store: &Store) -> std::io::Result<usize> {
        Ok(std::fs::read_dir(store.witness_dir())?.count())
    }

    /// The sweep judges a witness by its sidecar and its files' metadata; the
    /// body is never opened. Proved by a body that is not a witness at all
    /// surviving a sweep because its sidecar is in order. The price of that
    /// rule is stated here too: a corrupt witness is found at lookup, where
    /// it is refused and complained about, not by the sweep.
    #[test]
    fn a_sweep_never_reads_a_witness_body() -> std::io::Result<()> {
        let dir = scratch("witness-body-unread");
        let store = Store::open(&dir)?;
        let witness = crate::readset::DirWitness::open(store.witness_dir())?;
        let id = identity(b"garbage body");
        witness.put(&id, &[])?;
        std::fs::write(witness.path(&id), b"not canon, not read")?;

        let report = store.sweep(u64::MAX)?;
        let left = witness_files(&store)?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(report.witnesses_removed, 0, "{report:?}");
        assert_eq!(report.witnesses_unreadable, 0, "{report:?}");
        assert_eq!(left, 2, "witness and sidecar");
        Ok(())
    }

    /// A witness without a readable sidecar is a format this build did not
    /// write. It can never hit, and it is counted apart from ordinary
    /// reclamation because a run where every witness is unreadable is a
    /// broken format and not a tidy store (ENG-12601).
    #[test]
    fn a_witness_without_its_sidecar_is_reclaimed_as_unreadable() -> std::io::Result<()> {
        let dir = scratch("witness-no-sidecar");
        let store = Store::open(&dir)?;
        fill(&store, 2)?;
        let witness = crate::readset::DirWitness::open(store.witness_dir())?;
        // Two shapes of "no readable sidecar": none at all, and one whose
        // lines are not object names.
        witness.put(&identity(b"no sidecar"), &[])?;
        std::fs::remove_file(witness.refs_path(&identity(b"no sidecar")))?;
        witness.put(&identity(b"bad sidecar"), &[])?;
        std::fs::write(
            witness.refs_path(&identity(b"bad sidecar")),
            b"not a name\n",
        )?;
        // And the pre-sidecar signature: a stray body under a witness name.
        std::fs::write(store.witness_dir().join("a".repeat(64)), b"not canon")?;

        let report = store.sweep(u64::MAX)?;
        let left = witness_files(&store)?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(report.witnesses_removed, 3, "{report:?}");
        assert_eq!(report.witnesses_unreadable, 3, "{report:?}");
        assert_eq!(report.witnesses_left, 0, "{report:?}");
        assert_eq!(left, 0, "the bad sidecar must go with its witness");
        Ok(())
    }

    /// A witness whose sidecar names an object the store no longer holds
    /// fails replay on a missing ATerm every time, so it is dead, and dead
    /// before the object pass so the objects it alone kept alive go in the
    /// same sweep.
    #[test]
    fn a_witness_naming_a_missing_object_is_reclaimed_with_what_it_kept_alive()
    -> std::io::Result<()> {
        let dir = scratch("witness-missing-object");
        let store = Store::open(&dir)?;
        let cas = DirCas::open(store.objects_dir()).map_err(std::io::Error::other)?;
        let kept = cas
            .put(b"Derive([], kept only by the dead witness)")
            .map_err(std::io::Error::other)?;
        let gone = cas
            .put(b"Derive([], removed behind the witness's back)")
            .map_err(std::io::Error::other)?;
        let witness = crate::readset::DirWitness::open(store.witness_dir())?;
        let id = identity(b"names a missing object");
        witness.put(&id, &[derivation_row(kept), derivation_row(gone)])?;
        std::fs::remove_file(store.objects_dir().join(gone.hash().to_hex()))?;

        let report = store.sweep(u64::MAX)?;
        let kept_left = cas.has(kept).map_err(std::io::Error::other)?;
        let witness_left = witness.get(&id);
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(report.witnesses_removed, 1, "{report:?}");
        assert_eq!(report.witnesses_unreadable, 0, "{report:?}");
        assert_eq!(report.objects_removed, 1, "{report:?}");
        assert!(
            !kept_left,
            "the dead witness kept its ATerm alive for another sweep"
        );
        assert_eq!(witness_left, crate::readset::WitnessLookup::Missing);
        Ok(())
    }

    /// The sweep reports enough to tell housekeeping from destruction.
    ///
    /// ENG-12601 printed "swept 0 rows, 0 objects, 5 witnesses" and read as
    /// tidying up. What made it destruction rather than tidying is that
    /// nothing was left, and the report could not say so: `witnesses_removed`
    /// alone is the same number whether four of forty went or all five of
    /// five. The two extra fields are what let a caller shout, and
    /// `eval-server` does.
    #[test]
    fn a_sweep_that_empties_the_witness_store_says_so() -> std::io::Result<()> {
        let dir = scratch("witness-report");
        let store = Store::open(&dir)?;
        fill(&store, 2)?;
        let witness = crate::readset::DirWitness::open(store.witness_dir())?;
        // Two witnesses naming an object nobody stored: ordinary reclamation.
        let absent = ix_kernel::ObjId::of(b"never stored");
        for name in [b"one".as_slice(), b"two".as_slice()] {
            witness.put(&identity(name), &[derivation_row(absent)])?;
        }
        // And one without a sidecar, which is the ENG-12601 signature rather
        // than ordinary reclamation.
        std::fs::write(store.witness_dir().join("a".repeat(64)), b"not canon")?;

        let report = store.sweep(u64::MAX)?;
        drop(std::fs::remove_dir_all(&dir));

        assert_eq!(report.witnesses_removed, 3, "{report:?}");
        assert_eq!(report.witnesses_left, 0, "{report:?}");
        assert_eq!(
            report.witnesses_unreadable, 1,
            "an unreadable witness must be counted apart from a reclaimed one, \
             because a run where every witness is unreadable is a broken format \
             and not a tidy store: {report:?}"
        );
        Ok(())
    }

    /// The assertion whose absence let ENG-12601 through: a witness whose
    /// objects are all present must survive a sweep that is under its cap.
    ///
    /// Every witness test here was a removal test, so a rule that removed
    /// *everything* satisfied all of them. That is what shipped: the sweep
    /// judged a witness by its filename, the filename stopped being the
    /// module's object address, and every sweep emptied the witness
    /// directory. A capped store then served nothing while reporting itself
    /// under cap and healthy -- arm E of rust-incremental-gate went from 10
    /// hits of 11 to 0, and nothing else noticed.
    #[test]
    fn a_witness_whose_objects_are_present_survives_a_sweep() -> std::io::Result<()> {
        let dir = scratch("witness-live");
        let store = Store::open(&dir)?;
        let cas = DirCas::open(store.objects_dir()).map_err(std::io::Error::other)?;
        fill(&store, 2)?;
        let aterm = cas
            .put(b"Derive([], live)")
            .map_err(std::io::Error::other)?;
        let witness = crate::readset::DirWitness::open(store.witness_dir())?;
        let id = identity(b"live");
        witness.put(&id, &[derivation_row(aterm)])?;

        let report = store.sweep(u64::MAX)?;
        let survived = matches!(witness.get(&id), crate::readset::WitnessLookup::Found(_));
        let aterm_left = cas.has(aterm).map_err(std::io::Error::other)?;
        let left = witness_files(&store)?;
        drop(std::fs::remove_dir_all(&dir));

        assert_eq!(
            report.witnesses_removed, 0,
            "the sweep reclaimed a witness whose objects are all here: {report:?}"
        );
        assert_eq!(report.objects_removed, 0, "{report:?}");
        assert_eq!(left, 2, "witness and sidecar");
        assert!(
            survived,
            "the witness file is gone, so every later process starts cold"
        );
        assert!(
            aterm_left,
            "the ATerm a live witness names was swept from under it"
        );
        Ok(())
    }

    /// Over the cap, witnesses and rows are one LRU: the least recently used
    /// witness goes first and takes the objects only it named, while a
    /// witness a hit touched since survives with its objects, even though it
    /// was written earlier.
    #[test]
    fn over_the_cap_the_least_recently_used_witness_goes_first_with_its_objects()
    -> std::io::Result<()> {
        let dir = scratch("witness-lru");
        let store = Store::open(&dir)?;
        let cas = DirCas::open(store.objects_dir()).map_err(std::io::Error::other)?;
        let witness = crate::readset::DirWitness::open(store.witness_dir())?;
        let mut objects = Vec::new();
        for label in [b"touched".as_slice(), b"untouched".as_slice()] {
            let mut payload = vec![b'a'; 4096];
            payload.extend_from_slice(label);
            let aterm = cas.put(&payload).map_err(std::io::Error::other)?;
            witness.put(&identity(label), &[derivation_row(aterm)])?;
            objects.push(aterm);
        }
        // Age both, then mark only the first as used now. Without the
        // explicit ageing this depends on filesystem timestamp granularity.
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for label in [b"touched".as_slice(), b"untouched".as_slice()] {
            std::fs::File::open(witness.path(&identity(label)))?.set_modified(old)?;
        }
        assert!(witness.touch(&identity(b"touched")));

        // A cap that fits one witness with its object but not both.
        let report = store.sweep(store.size()? - 1)?;
        let touched_left = matches!(
            witness.get(&identity(b"touched")),
            crate::readset::WitnessLookup::Found(_)
        );
        let untouched_left = matches!(
            witness.get(&identity(b"untouched")),
            crate::readset::WitnessLookup::Found(_)
        );
        let touched_object = cas.has(objects[0]).map_err(std::io::Error::other)?;
        let untouched_object = cas.has(objects[1]).map_err(std::io::Error::other)?;
        let after = store.size()?;
        drop(std::fs::remove_dir_all(&dir));

        assert_eq!(report.witnesses_removed, 1, "{report:?}");
        assert_eq!(report.objects_removed, 1, "{report:?}");
        assert!(
            touched_left && touched_object,
            "the touched witness or its object went: {report:?}"
        );
        assert!(
            !untouched_left && !untouched_object,
            "the untouched witness or its object stayed: {report:?}"
        );
        assert_eq!(
            after, report.bytes_after,
            "the report's arithmetic disagrees with the directory"
        );
        Ok(())
    }

    /// A store with no cap, or under its cap, is left alone by
    /// `sweep_to_cap`; one over its cap is swept to it. This is the call
    /// `ResultCache::record` makes after every publication.
    #[test]
    fn sweep_to_cap_acts_only_on_a_capped_store_over_its_cap() -> std::io::Result<()> {
        let dir = scratch("sweep-to-cap");
        let uncapped = Store::open(&dir)?;
        // Rows in the eval domain, which is one of the two `sweep_to_cap` names.
        let cas = DirCas::open(uncapped.objects_dir()).map_err(std::io::Error::other)?;
        for i in 0..8_u8 {
            let mut payload = vec![b'p'; 1024];
            payload.push(i);
            let output = cas.put(&payload).map_err(std::io::Error::other)?;
            uncapped
                .rows()
                .put(crate::readset::eval_domain(), &[i], output)
                .map_err(std::io::Error::other)?;
        }
        let size = uncapped.size()?;
        let none = uncapped.sweep_to_cap()?;
        let under = Store::open(&dir)?.with_max_bytes(size).sweep_to_cap()?;
        let over = Store::open(&dir)?.with_max_bytes(size / 2).sweep_to_cap()?;
        let after = uncapped.size()?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(none, None, "an uncapped store was swept");
        assert_eq!(under, None, "a store under its cap was swept");
        let report = over.expect("a store over its cap was not swept");
        assert!(report.rows_removed > 0, "{report:?}");
        assert!(
            after <= size / 2,
            "{after} bytes with a cap of {}",
            size / 2
        );
        Ok(())
    }

    /// Every file in the swept directories is one of the census's classes,
    /// so a cap the sweep is allowed to reach it does reach: the LRU can
    /// remove every row and witness, after which every object is
    /// unreferenced. A cap of one byte therefore empties the store.
    #[test]
    fn a_cap_below_everything_empties_the_store() -> std::io::Result<()> {
        let dir = scratch("empty-to-cap");
        let store = Store::open(&dir)?;
        fill(&store, 2)?;
        let report = store.sweep(1)?;
        let after = store.size()?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(report.bytes_after, 0, "{report:?}");
        assert_eq!(
            after, 0,
            "the report says empty and the directory disagrees"
        );
        Ok(())
    }

    /// Rows of every domain root their objects and share one LRU. The first
    /// sweep counted references only from the domains it was told about, so
    /// an object named by a row of another domain was reclaimed while its row
    /// stayed, pointing at nothing; the control here is that same object
    /// surviving an uncapped sweep.
    #[test]
    fn rows_of_every_domain_root_their_objects_and_share_the_lru() -> std::io::Result<()> {
        let dir = scratch("every-domain");
        let store = Store::open(&dir)?;
        fill(&store, 10)?;
        let other = Domain::mint("test.effect", "other-op");
        let cas = DirCas::open(store.objects_dir()).map_err(std::io::Error::other)?;
        let object = cas
            .put(b"named only by the other domain")
            .map_err(std::io::Error::other)?;
        store
            .rows()
            .put(other, b"other request", object)
            .map_err(std::io::Error::other)?;

        let uncapped = store.sweep(u64::MAX)?;
        let object_after_uncapped = cas.has(object).map_err(std::io::Error::other)?;
        let emptied = store.sweep(1)?;
        let other_rows_left =
            std::fs::read_dir(store.index_dir().join(other.hash().to_hex()))?.count();
        drop(std::fs::remove_dir_all(&dir));

        assert_eq!(uncapped.objects_removed, 0, "{uncapped:?}");
        assert!(
            object_after_uncapped,
            "an object rooted by another domain's row was reclaimed"
        );
        assert_eq!(
            emptied.rows_removed, 11,
            "rows of every domain are one LRU: {emptied:?}"
        );
        assert_eq!(other_rows_left, 0);
        Ok(())
    }

    /// Temporaries of interrupted writes and sidecars without a witness take
    /// space the cap has to account for and can never be live, so a sweep
    /// removes and counts them. Before this they were skipped by the listing
    /// and counted by the size, which made the cap unreachable by exactly
    /// their bytes without saying so.
    #[test]
    fn leftovers_are_reclaimed_and_counted() -> std::io::Result<()> {
        let dir = scratch("leftovers");
        let store = Store::open(&dir)?;
        fill(&store, 2)?;
        std::fs::write(store.objects_dir().join(".tmp-1-1"), b"half an object")?;
        std::fs::write(
            store
                .index_dir()
                .join(domain().hash().to_hex())
                .join(".tmp-1-2"),
            b"half a row",
        )?;
        std::fs::write(store.witness_dir().join(".tmp-1-3"), b"half a witness")?;
        std::fs::write(
            store.witness_dir().join(format!("{}.refs", "b".repeat(64))),
            b"",
        )?;
        let before = store.size()?;
        let census_leftovers = store.census()?.leftovers();

        let report = store.sweep(u64::MAX)?;
        let after = store.size()?;
        let witness_files = witness_files(&store)?;
        drop(std::fs::remove_dir_all(&dir));

        assert_eq!(census_leftovers, 4);
        assert_eq!(report.leftovers_removed, 4, "{report:?}");
        assert_eq!(
            report.bytes_before, before,
            "the size and the census disagree"
        );
        assert_eq!(after, report.bytes_after);
        assert_eq!(
            before - after,
            ("half an object".len() + "half a row".len() + "half a witness".len()) as u64,
            "the leftovers' bytes were not the bytes freed: {report:?}"
        );
        assert_eq!(witness_files, 0);
        Ok(())
    }

    /// A sidecar that is not UTF-8 is a sidecar this build did not write. It
    /// marks its witness unreadable, like a missing one; it does not abort
    /// the sweep, which would leave the whole store unbounded behind one bad
    /// file.
    #[test]
    fn a_sidecar_that_is_not_text_marks_its_witness_unreadable() -> std::io::Result<()> {
        let dir = scratch("sidecar-binary");
        let store = Store::open(&dir)?;
        let witness = crate::readset::DirWitness::open(store.witness_dir())?;
        let id = identity(b"binary sidecar");
        witness.put(&id, &[])?;
        std::fs::write(witness.refs_path(&id), [0xff_u8, 0xfe, 0x00])?;

        let dead = store.census()?.dead_witnesses();
        let report = store.sweep(u64::MAX)?;
        let left = witness_files(&store)?;
        drop(std::fs::remove_dir_all(&dir));

        assert_eq!(dead, vec![(id.as_hash().to_hex(), DeadWitness::Unreadable)]);
        assert_eq!(report.witnesses_unreadable, 1, "{report:?}");
        assert_eq!(report.witnesses_removed, 1, "{report:?}");
        assert_eq!(left, 0, "the unreadable sidecar must go with its witness");
        Ok(())
    }

    /// The census and the sweep are one rule: what the census calls dead or
    /// unreferenced is exactly what an uncapped sweep removes. This is the
    /// guard against ENG-12884, a scrub that kept an old rule after the sweep
    /// changed.
    #[test]
    fn the_census_predicts_the_sweep() -> std::io::Result<()> {
        let dir = scratch("census-predicts");
        let store = Store::open(&dir)?;
        let cas = DirCas::open(store.objects_dir()).map_err(std::io::Error::other)?;
        fill(&store, 2)?;
        let orphan = cas
            .put(b"nobody names this")
            .map_err(std::io::Error::other)?;
        let live = cas
            .put(b"Derive([], live)")
            .map_err(std::io::Error::other)?;
        let gone = cas
            .put(b"Derive([], gone)")
            .map_err(std::io::Error::other)?;
        let witness = crate::readset::DirWitness::open(store.witness_dir())?;
        witness.put(&identity(b"live"), &[derivation_row(live)])?;
        witness.put(&identity(b"dead"), &[derivation_row(gone)])?;
        std::fs::remove_file(store.objects_dir().join(gone.hash().to_hex()))?;

        let census = store.census()?;
        let dead = census.dead_witnesses();
        let unreferenced = census.unreferenced_objects();
        let report = store.sweep(u64::MAX)?;
        let live_left = cas.has(live).map_err(std::io::Error::other)?;
        drop(std::fs::remove_dir_all(&dir));

        assert_eq!(
            dead,
            vec![(
                identity(b"dead").as_hash().to_hex(),
                DeadWitness::MissingObject(gone.hash().to_hex())
            )]
        );
        assert_eq!(unreferenced, vec![orphan.hash().to_hex()]);
        assert_eq!(report.witnesses_removed, dead.len(), "{report:?}");
        assert_eq!(report.objects_removed, unreferenced.len(), "{report:?}");
        assert!(
            live_left,
            "the census called it live and the sweep removed it"
        );
        Ok(())
    }

    #[test]
    fn sweeping_an_empty_store_is_a_no_op() -> std::io::Result<()> {
        let dir = scratch("empty");
        let store = Store::open(&dir)?;
        let report = store.sweep(0)?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(report, SweepReport::default());
        Ok(())
    }

    #[test]
    fn a_live_witness_keeps_its_aterm_object() -> std::io::Result<()> {
        let dir = scratch("witness-aterm");
        let store = Store::open(&dir)?;
        let cas = DirCas::open(store.objects_dir()).map_err(std::io::Error::other)?;
        let module = cas
            .put(b"module with a derivation witness")
            .map_err(std::io::Error::other)?;
        let aterm = cas.put(b"Derive([])").map_err(std::io::Error::other)?;
        let rows = DirRows::open(store.index_dir()).map_err(std::io::Error::other)?;
        rows.put(domain(), b"module row", module)
            .map_err(std::io::Error::other)?;
        let identity = crate::readset::EvalId::of(
            module.hash(),
            &crate::eval::Settings::default(),
            &crate::session::Arguments::none(),
            &whole_question(),
        );
        let witness = crate::readset::DirWitness::open(store.witness_dir())?;
        witness.put(
            &identity,
            &[(
                crate::readset::Question::WriteDrv {
                    name: "kept".to_owned(),
                    answer: crate::readset::WriteDrvAnswer::Written(
                        "/nix/store/kept.drv".to_owned(),
                    ),
                    aterm,
                },
                crate::readset::Recorded::Digest(ix_kernel::Hash::from_bytes([0; 32])),
            )],
        )?;

        store.sweep(u64::MAX)?;
        assert_eq!(
            cas.get(aterm).map_err(std::io::Error::other)?,
            Some(b"Derive([])".to_vec()),
            "the orphan-object pass removed an ATerm still named by a live witness"
        );
        drop(std::fs::remove_dir_all(&dir));
        Ok(())
    }

    /// The under-cap check reads sizes from `objects.sizes`, not from a stat
    /// per object: a memo that lies is believed by `size`, and the measured
    /// census `sweep_to_cap` takes when the memo says "over" corrects it
    /// without removing anything. The lie is the control: a check that
    /// stat'ed every object would report the real total at `believed`.
    #[test]
    fn the_under_cap_check_reads_the_object_size_memo_and_a_sweep_corrects_it()
    -> std::io::Result<()> {
        let dir = scratch("sizes-memo");
        let store = Store::open(&dir)?;
        fill(&store, 3)?;
        let real = store.size()?;
        let memo_path = dir.join(OBJECT_SIZES);
        let memo = std::fs::read_to_string(&memo_path)?;
        let (name, bytes) = memo
            .lines()
            .nth(1)
            .and_then(|line| line.split_once('\t'))
            .expect("a memo line after the header");
        let lie = memo.replace(
            &format!("{name}\t{bytes}"),
            &format!(
                "{name}\t{}",
                bytes.parse::<u64>().expect("a size") + 1_000_000
            ),
        );
        std::fs::write(&memo_path, lie)?;
        let believed = store.size()?;
        let swept = Store::open(&dir)?.with_max_bytes(real).sweep_to_cap()?;
        let corrected = store.size()?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(
            believed,
            real + 1_000_000,
            "the memo was not what `size` read"
        );
        assert_eq!(swept, None, "a store under its measured cap was swept");
        assert_eq!(
            corrected, real,
            "the measured census did not rewrite the memo"
        );
        Ok(())
    }

    /// A new object is measured once and remembered; an object removed behind
    /// the memo's back is forgotten. Both totals match a stat of every file.
    #[test]
    fn the_object_size_memo_follows_the_objects_directory() -> std::io::Result<()> {
        let dir = scratch("sizes-follow");
        let store = Store::open(&dir)?;
        fill(&store, 2)?;
        let two = store.size()?;
        let cas = DirCas::open(store.objects_dir()).map_err(std::io::Error::other)?;
        let added = cas.put(&vec![b'q'; 4096]).map_err(std::io::Error::other)?;
        let three = store.size()?;
        std::fs::remove_file(store.objects_dir().join(added.hash().to_hex()))?;
        let back = store.size()?;
        let memo = std::fs::read_to_string(dir.join(OBJECT_SIZES))?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(three, two + 4096);
        assert_eq!(back, two);
        assert!(!memo.contains(&added.hash().to_hex()), "{memo}");
        Ok(())
    }

    /// A memo this build cannot read is measured past and rewritten, never
    /// half-trusted.
    #[test]
    fn an_unreadable_object_size_memo_is_measured_past_and_rewritten() -> std::io::Result<()> {
        let dir = scratch("sizes-corrupt");
        let store = Store::open(&dir)?;
        fill(&store, 2)?;
        let real = store.size()?;
        std::fs::write(dir.join(OBJECT_SIZES), "not a memo\nabc\tnot a number\n")?;
        let after = store.size()?;
        let memo = std::fs::read_to_string(dir.join(OBJECT_SIZES))?;
        drop(std::fs::remove_dir_all(&dir));
        assert_eq!(after, real);
        assert!(memo.starts_with(OBJECT_SIZES_HEADER), "{memo}");
        Ok(())
    }
}
