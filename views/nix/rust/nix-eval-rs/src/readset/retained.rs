//! Decoded witness payloads retained across CAPI sessions sharing one explicit cache owner.
//!
//! The byte limit is an accounted payload budget, not an RSS measurement. It
//! charges vector and string capacities, boxed values, reference-counted values
//! and their counters, map key/value storage, and per-entry metadata. Shared
//! allocations are charged at every occurrence, conservatively. No witness is
//! serialized or cloned to measure it.
//!
//! Allocator bookkeeping, BTreeMap internal nodes and unused LRU backing slots
//! are not observable through their public APIs and are excluded. They remain
//! bounded: map entries are charged individually, all variable-sized allocations
//! are charged, and the LRU never holds more than max_entries entries. Its backing
//! capacity can retain its previous high-water mark after removal. Cloned Entry
//! handles held by an active replay can outlive eviction; their lifetime belongs
//! to that replay, outside this cache's retention budget.

use super::{EvalId, Question, Recorded, WitnessRow, WriteDrvAnswer};
use crate::task::{FetchRequest, FetchTreeRequest, FilteredCopy, TreeAttr};
use crate::value2::{ContextElem, PathValue, Root};
use ix_kernel::hash::Hash;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub(crate) type Shared = Rc<RefCell<Retained>>;

#[derive(Clone)]
pub(crate) struct Entry {
    pub rows: Rc<Vec<WitnessRow>>,
    pub shape: Hash,
    pub slot: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub memory_hits: u64,
    pub disk_loads: u64,
    /// Accounted payload bytes, with exclusions documented at module level.
    pub retained_bytes: u64,
    pub entries: u64,
    /// Entries displaced by capacity pressure; explicit removal is separate.
    pub evictions: u64,
}

struct Cached {
    store: PathBuf,
    identity: EvalId,
    entry: Entry,
    bytes: u64,
}

pub(crate) struct Retained {
    // Oldest first. Sessions keep a small bounded set, so linear key lookup
    // avoids a second index retaining duplicate paths and identities.
    entries: VecDeque<Cached>,
    max_bytes: u64,
    max_entries: usize,
    stats: Stats,
}

impl Retained {
    pub(crate) fn new(max_bytes: u64, max_entries: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_bytes,
            max_entries,
            stats: Stats::default(),
        }
    }

    /// Clone the handle out before replaying host questions. No RefCell borrow
    /// needs to remain live while a nested evaluation accesses the same cache.
    pub(crate) fn get(&mut self, store: &Path, identity: &EvalId) -> Option<Entry> {
        let position = self.position(store, identity)?;
        let cached = self.entries.remove(position)?;
        let entry = cached.entry.clone();
        self.entries.push_back(cached);
        self.stats.memory_hits = self.stats.memory_hits.saturating_add(1);
        Some(entry)
    }

    /// Replacement invalidates the old entry even when the new one cannot fit.
    /// Otherwise an oversize new recipe would leave the previous one preferred.
    pub(crate) fn insert(&mut self, store: PathBuf, identity: EvalId, entry: Entry) {
        // Publishing the same decoded payload after another successful replay
        // must not walk hundreds of megabytes merely to recount allocations.
        if let Some(position) = self.position(&store, &identity)
            && self
                .entries
                .get(position)
                .is_some_and(|cached| Rc::ptr_eq(&cached.entry.rows, &entry.rows))
            && let Some(mut cached) = self.entries.remove(position)
        {
            cached.entry = entry;
            self.entries.push_back(cached);
            return;
        }
        let bytes = entry_bytes(&store, &entry);
        self.remove(&store, &identity);
        if self.max_entries == 0 || bytes == u64::MAX || bytes > self.max_bytes {
            return;
        }

        while self.entries.len() >= self.max_entries
            || self.stats.retained_bytes > self.max_bytes - bytes
        {
            let Some(oldest) = self.entries.pop_front() else {
                return;
            };
            self.stats.retained_bytes -= oldest.bytes;
            self.stats.evictions = self.stats.evictions.saturating_add(1);
        }
        self.stats.retained_bytes += bytes;
        self.entries.push_back(Cached {
            store,
            identity,
            entry,
            bytes,
        });
        self.stats.entries = count(self.entries.len());
    }

    pub(crate) fn remove(&mut self, store: &Path, identity: &EvalId) {
        if let Some(position) = self.position(store, identity)
            && let Some(removed) = self.entries.remove(position)
        {
            self.stats.retained_bytes -= removed.bytes;
            self.stats.entries = count(self.entries.len());
        }
    }

    pub(crate) fn note_disk_load(&mut self) {
        self.stats.disk_loads = self.stats.disk_loads.saturating_add(1);
    }

    pub(crate) fn stats(&self) -> Stats {
        self.stats
    }

    fn position(&self, store: &Path, identity: &EvalId) -> Option<usize> {
        self.entries
            .iter()
            .position(|cached| cached.store == store && cached.identity == *identity)
    }
}

fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn sum(values: impl IntoIterator<Item = u64>) -> u64 {
    values.into_iter().fold(0, u64::saturating_add)
}

fn allocation<T>(capacity: usize) -> u64 {
    count(capacity).saturating_mul(count(size_of::<T>()))
}

fn rc_bytes(payload: u64) -> u64 {
    // Two Rc counters plus alignment slack before the payload.
    sum([payload, allocation::<usize>(3)])
}

fn string_bytes(value: &String) -> u64 {
    count(value.capacity())
}

fn optional_string_bytes(value: &Option<String>) -> u64 {
    value.as_ref().map_or(0, string_bytes)
}

fn path_bytes(path: &PathValue) -> u64 {
    sum([
        rc_bytes(count(size_of::<PathValue>())),
        rc_bytes(count(path.path.len())),
        match &path.root {
            Root::Ambient => 0,
            Root::Mounted(root) => rc_bytes(count(root.len())),
        },
    ])
}

fn tree_attrs_bytes(attrs: &BTreeMap<String, TreeAttr>) -> u64 {
    sum(attrs.iter().map(|(name, attr)| {
        sum([
            count(size_of::<(String, TreeAttr)>()),
            string_bytes(name),
            match attr {
                TreeAttr::Str(value) => string_bytes(value),
                TreeAttr::Bool(_) | TreeAttr::Int(_) => 0,
            },
        ])
    }))
}

fn question_bytes(question: &Question) -> u64 {
    match question {
        Question::Import(path)
        | Question::ReadFile(path)
        | Question::ReadFileBytes(path)
        | Question::ReadDir(path)
        | Question::PathExists(path)
        | Question::DirExists(path)
        | Question::FileType(path)
        | Question::FileTypeResolved(path)
        | Question::CopyToStore(path)
        | Question::StorePath(path)
        | Question::Tree(path) => path_bytes(path),
        Question::GetEnv(value)
        | Question::EnsurePath(value)
        | Question::LockFlake(value)
        | Question::ParseFlakeRef(value) => string_bytes(value),
        Question::FindFile { entries, name } => sum([
            allocation::<crate::task::SearchPathEntry>(entries.capacity()),
            string_bytes(name),
            sum(entries
                .iter()
                .map(|entry| sum([string_bytes(&entry.prefix), string_bytes(&entry.path)]))),
        ]),
        Question::NixPath => 0,
        Question::StoreText {
            name,
            contents,
            references,
        } => sum([
            string_bytes(name),
            string_bytes(contents),
            allocation::<String>(references.capacity()),
            sum(references.iter().map(string_bytes)),
        ]),
        Question::WriteDrv {
            name,
            answer,
            aterm: _,
        } => sum([
            string_bytes(name),
            match answer {
                WriteDrvAnswer::Written(path) => string_bytes(path),
                WriteDrvAnswer::Failed { outcome, detail } => {
                    sum([string_bytes(outcome), string_bytes(detail)])
                }
            },
        ]),
        Question::Realise(context) => sum([
            allocation::<ContextElem>(context.capacity()),
            sum(context.iter().map(|element| match element {
                ContextElem::Opaque(path) | ContextElem::DrvDeep(path) => {
                    rc_bytes(count(path.len()))
                }
                ContextElem::Built { drv, output } => {
                    sum([rc_bytes(count(drv.len())), rc_bytes(count(output.len()))])
                }
            })),
        ]),
        Question::StoreFiltered(request) => sum([
            count(size_of::<FilteredCopy>()),
            path_bytes(&request.root),
            string_bytes(&request.name),
            optional_string_bytes(&request.expected_sha256),
            request.accepted.as_ref().map_or(0, |accepted| {
                sum([
                    allocation::<crate::task::AcceptedPath>(accepted.capacity()),
                    sum(accepted.iter().map(|entry| string_bytes(&entry.path))),
                ])
            }),
        ]),
        Question::Fetch(request) => sum([
            count(size_of::<FetchRequest>()),
            string_bytes(&request.url),
            string_bytes(&request.name),
            optional_string_bytes(&request.expected_sha256),
        ]),
        Question::FetchTree(request) => sum([
            count(size_of::<FetchTreeRequest>()),
            tree_attrs_bytes(&request.attrs),
        ]),
        Question::FlakeRefToString(attrs) => tree_attrs_bytes(attrs),
    }
}

fn entry_bytes(store: &PathBuf, entry: &Entry) -> u64 {
    sum([
        count(size_of::<Cached>()),
        count(store.capacity()),
        rc_bytes(count(size_of::<Vec<WitnessRow>>())),
        allocation::<WitnessRow>(entry.rows.capacity()),
        sum(entry.rows.iter().map(|(question, recorded)| {
            sum([
                question_bytes(question),
                match recorded {
                    Recorded::Digest(_) => 0,
                    Recorded::Answer(value) => string_bytes(value),
                },
            ])
        })),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(label: u8) -> EvalId {
        EvalId {
            key: Hash::from_bytes([label; 32]),
        }
    }

    fn entry(label: u8) -> Entry {
        Entry {
            rows: Rc::new(vec![(
                Question::GetEnv(format!("variable-{label}")),
                Recorded::Digest(Hash::from_bytes([label; 32])),
            )]),
            shape: Hash::from_bytes([label; 32]),
            slot: usize::from(label) % 4,
        }
    }

    #[test]
    fn item_limit_evicts_the_least_recently_read_entry() {
        let store = PathBuf::from("/cache");
        let mut retained = Retained::new(u64::MAX, 2);
        retained.insert(store.clone(), identity(1), entry(1));
        retained.insert(store.clone(), identity(2), entry(2));
        assert!(retained.get(&store, &identity(1)).is_some());
        retained.insert(store.clone(), identity(3), entry(3));

        assert!(retained.get(&store, &identity(2)).is_none());
        assert!(retained.get(&store, &identity(1)).is_some());
        assert!(retained.get(&store, &identity(3)).is_some());
        assert_eq!(retained.stats().entries, 2);
        assert_eq!(retained.stats().evictions, 1);
        assert_eq!(retained.stats().memory_hits, 3);
    }

    #[test]
    fn byte_limit_evicts_even_when_the_item_limit_has_room() {
        let store = PathBuf::from("/cache");
        let first = entry(1);
        let second = entry(2);
        let limit = entry_bytes(&store, &first) + entry_bytes(&store, &second) - 1;
        let second_bytes = entry_bytes(&store, &second);
        let mut retained = Retained::new(limit, 10);
        retained.insert(store.clone(), identity(1), first);
        retained.insert(store.clone(), identity(2), second);

        assert!(retained.get(&store, &identity(1)).is_none());
        assert!(retained.get(&store, &identity(2)).is_some());
        assert_eq!(retained.stats().retained_bytes, second_bytes);
        assert_eq!(retained.stats().evictions, 1);
    }

    #[test]
    fn replacement_updates_one_identity_and_oversize_replacement_removes_it() {
        let store = PathBuf::from("/cache");
        let replacement = entry(2);
        let bytes = entry_bytes(&store, &replacement);
        let mut retained = Retained::new(bytes, 4);
        retained.insert(store.clone(), identity(1), entry(1));
        retained.insert(store.clone(), identity(1), replacement);
        let served = retained.get(&store, &identity(1));
        assert_eq!(served.as_ref().map(|entry| entry.slot), Some(2));
        assert_eq!(
            served.as_ref().map(|entry| entry.shape),
            Some(Hash::from_bytes([2; 32]))
        );
        assert_eq!(retained.stats().entries, 1);
        assert_eq!(retained.stats().retained_bytes, bytes);

        let mut text = String::with_capacity(8192);
        text.push('x');
        let huge = Entry {
            rows: Rc::new(vec![(
                Question::GetEnv(text),
                Recorded::Digest(Hash::from_bytes([0; 32])),
            )]),
            shape: Hash::from_bytes([3; 32]),
            slot: 3,
        };
        let weak = Rc::downgrade(&huge.rows);
        retained.insert(store.clone(), identity(1), huge);
        assert!(retained.get(&store, &identity(1)).is_none());
        assert!(
            weak.upgrade().is_none(),
            "oversize payload remained retained"
        );
        assert_eq!(retained.stats().entries, 0);
        assert_eq!(retained.stats().retained_bytes, 0);
        assert_eq!(retained.stats().evictions, 0);
    }

    #[test]
    fn stores_are_separate_and_removal_releases_only_the_requested_entry() {
        let first = PathBuf::from("/cache-one");
        let second = PathBuf::from("/cache-two");
        let mut retained = Retained::new(u64::MAX, 4);
        retained.insert(first.clone(), identity(1), entry(1));
        retained.insert(second.clone(), identity(1), entry(2));
        retained.note_disk_load();
        retained.note_disk_load();
        retained.remove(&first, &identity(1));

        assert!(retained.get(&first, &identity(1)).is_none());
        assert_eq!(
            retained.get(&second, &identity(1)).map(|entry| entry.slot),
            Some(2)
        );
        assert_eq!(retained.stats().disk_loads, 2);
        assert_eq!(retained.stats().entries, 1);
        retained.remove(&second, &identity(1));
        retained.remove(&second, &identity(1));
        assert_eq!(retained.stats().retained_bytes, 0);
    }

    #[test]
    fn a_replay_handle_survives_eviction_without_holding_a_cache_borrow() {
        let store = PathBuf::from("/cache");
        let retained: Shared = Rc::new(RefCell::new(Retained::new(u64::MAX, 1)));
        let original = entry(1);
        let weak = Rc::downgrade(&original.rows);
        retained
            .borrow_mut()
            .insert(store.clone(), identity(1), original);
        let replay = retained.borrow_mut().get(&store, &identity(1));
        retained.borrow_mut().insert(store, identity(2), entry(2));
        assert!(
            weak.upgrade().is_some(),
            "eviction invalidated an active replay"
        );
        drop(retained);
        assert!(
            weak.upgrade().is_some(),
            "session destruction invalidated an active replay"
        );
        drop(replay);
        assert!(
            weak.upgrade().is_none(),
            "payload leaked after its last replay ended"
        );
    }

    #[test]
    fn zero_limits_disable_retention() {
        for (bytes, entries) in [(0, 4), (u64::MAX, 0)] {
            let mut retained = Retained::new(bytes, entries);
            let payload = entry(1);
            let weak = Rc::downgrade(&payload.rows);
            retained.insert(PathBuf::from("/cache"), identity(1), payload);
            assert_eq!(retained.stats(), Stats::default());
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn accounting_includes_unused_capacities_and_nested_payloads() {
        let store = PathBuf::from("/cache");
        let mut rows = Vec::with_capacity(128);
        let mut contents = String::with_capacity(8192);
        contents.push('x');
        let mut references = Vec::with_capacity(64);
        let mut reference = String::with_capacity(4096);
        reference.push('r');
        references.push(reference);
        let mut answer = String::with_capacity(16384);
        answer.push('a');
        rows.push((
            Question::StoreText {
                name: "name".to_owned(),
                contents,
                references,
            },
            Recorded::Answer(answer),
        ));
        let accounted_minimum =
            allocation::<WitnessRow>(128) + allocation::<String>(64) + 8192 + 4096 + 16384;
        let payload = Entry {
            rows: Rc::new(rows),
            shape: Hash::from_bytes([0; 32]),
            slot: 0,
        };
        assert!(entry_bytes(&store, &payload) >= accounted_minimum);

        let mut retained = Retained::new(accounted_minimum - 1, 8);
        retained.insert(store, identity(1), payload);
        assert_eq!(retained.stats().entries, 0);
    }

    #[test]
    fn nested_path_and_fetch_allocations_are_charged() {
        let path = Rc::new(PathValue::new(Root::mounted("/mount"), "/mount/file"));
        let request = Question::StoreFiltered(Box::new(FilteredCopy {
            root: path,
            name: String::with_capacity(4096),
            method: crate::task::PathMethod::Flat,
            accepted: Some(vec![crate::task::AcceptedPath {
                path: String::with_capacity(8192),
                file_type: crate::host::FileType::Regular,
            }]),
            expected_sha256: Some(String::with_capacity(2048)),
            inherit_references: false,
        }));
        assert!(question_bytes(&request) >= 4096 + 8192 + 2048);
        let mut attrs = BTreeMap::new();
        attrs.insert(
            String::with_capacity(4096),
            TreeAttr::Str(String::with_capacity(8192)),
        );
        let tree = Question::FetchTree(Box::new(FetchTreeRequest {
            attrs,
            fetcher: crate::task::TreeFetcher::Tree,
        }));
        assert!(question_bytes(&tree) >= 4096 + 8192);
    }

    #[test]
    fn a_long_sequence_stays_inside_both_limits() {
        let store = PathBuf::from("/cache");
        let budget = entry_bytes(&store, &entry(1)) * 3;
        let mut retained = Retained::new(budget, 4);
        for label in 1..=100 {
            retained.insert(store.clone(), identity(label % 7), entry(label));
            let _ = retained.get(&store, &identity((label + 2) % 7));
            if label % 5 == 0 {
                retained.remove(&store, &identity(label % 7));
            }
            assert!(retained.stats().entries <= 4);
            assert!(retained.stats().retained_bytes <= budget);
            assert_eq!(retained.stats().entries, count(retained.entries.len()));
            assert_eq!(
                retained.stats().retained_bytes,
                sum(retained.entries.iter().map(|entry| entry.bytes))
            );
        }
        assert!(retained.stats().evictions > 0);
    }

    #[test]
    fn accounting_overflow_saturates_instead_of_admitting_an_undersized_charge() {
        assert_eq!(sum([u64::MAX, 1]), u64::MAX);
        assert_eq!(rc_bytes(u64::MAX), u64::MAX);
    }

    #[test]
    fn reinserting_the_same_payload_updates_its_slot_and_recency() {
        let store = PathBuf::from("/cache");
        let mut retained = Retained::new(u64::MAX, 2);
        let mut payload = entry(1);
        retained.insert(store.clone(), identity(1), payload.clone());
        retained.insert(store.clone(), identity(2), entry(2));
        let charged = retained.stats().retained_bytes;
        payload.slot = 3;
        retained.insert(store.clone(), identity(1), payload);
        assert_eq!(retained.stats().retained_bytes, charged);
        retained.insert(store.clone(), identity(3), entry(3));
        assert!(retained.get(&store, &identity(2)).is_none());
        assert_eq!(
            retained.get(&store, &identity(1)).map(|entry| entry.slot),
            Some(3)
        );
        assert_eq!(retained.stats().entries, 2);
        assert_eq!(retained.stats().evictions, 1);
    }
}
