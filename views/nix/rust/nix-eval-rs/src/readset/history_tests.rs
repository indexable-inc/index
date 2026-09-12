use super::*;
use std::cell::Cell;

type TestResult = Result<(), Box<dyn core::error::Error>>;

struct HistoryHost {
    version: Cell<u8>,
    selector_reads: Cell<usize>,
}

impl HistoryHost {
    fn new(version: u8) -> Self {
        Self {
            version: Cell::new(version),
            selector_reads: Cell::new(0),
        }
    }
}

impl Host for HistoryHost {
    crate::host::host_stubs!(settle);
    crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
    crate::host::host_stubs!(
        realise,
        store_text,
        write_derivation,
        store_filtered,
        fetch,
        lock_flake,
        fetch_tree,
        not_async,
        file_type_resolved,
        get_env,
        ensure_path,
        warn,
        find_file,
        nix_path,
        trace
    );

    fn read_file(&self, path: &crate::value2::PathValue) -> Result<String, String> {
        if path.path.as_ref() == "/selector" {
            self.selector_reads.set(self.selector_reads.get() + 1);
            Ok(self.version.get().to_string())
        } else {
            // Historical copied paths keep their content when the source changes.
            Ok(format!("contents of {}", path.path))
        }
    }

    fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
        self.read_file(path).map(String::into_bytes)
    }

    fn read_dir(
        &self,
        _path: &crate::value2::PathValue,
    ) -> Result<Vec<(String, FileType)>, String> {
        Ok(Vec::new())
    }

    fn path_exists_checked(&self, _path: &crate::value2::PathValue) -> Result<bool, String> {
        Ok(true)
    }

    fn dir_exists_checked(&self, _path: &crate::value2::PathValue) -> Result<bool, String> {
        Ok(false)
    }

    fn file_type(&self, _path: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
        Ok(Some(FileType::Regular))
    }

    fn copy_to_store(&self, _path: &crate::value2::PathValue) -> Result<String, StoreError> {
        Ok(format!(
            "/nix/store/{}-input",
            self.version.get().to_string().repeat(32)
        ))
    }
}

fn identity(label: &[u8]) -> EvalId {
    EvalId::of(
        &hash::tagged("history-test", &[label]),
        &crate::eval::Settings::default(),
        &crate::session::Arguments::none(),
        &crate::session::Question::Whole {
            render: crate::session::RenderMode::Plain,
        },
    )
}

fn answer(version: u8) -> EvalResult {
    EvalResult {
        status: "ok".to_owned(),
        value: version.to_string(),
        ..EvalResult::default()
    }
}

fn record_branch(store: &crate::store::Store, identity: &EvalId, host: &HistoryHost) -> TestResult {
    record_cached_branch(&mut ResultCache::persistent(store), identity, host)
}

fn record_cached_branch(
    cache: &mut ResultCache<'_, dyn Cas>,
    identity: &EvalId,
    host: &HistoryHost,
) -> TestResult {
    let recorder = RecordingHost::new(host);
    let selector = recorder.read_file(&crate::value2::ambient_path("/selector"))?;
    recorder.read_file(&crate::value2::ambient_path(format!("/branch/{selector}")))?;
    cache.record(
        identity,
        &recorder.take(),
        &answer(host.version.get()),
        host,
        &crate::eval::Settings::default(),
    )?;
    Ok(())
}

#[test]
fn reopened_cache_reuses_a_previous_dynamic_branch() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "branch-revert");
    let identity = identity(b"branch-revert");
    let host = HistoryHost::new(1);
    let settings = crate::eval::Settings::default();

    for (version, hit) in [(1, false), (1, true), (2, false), (2, true), (1, true)] {
        host.version.set(version);
        let store = crate::store::Store::open(&dir)?;
        let mut cache = ResultCache::persistent(&store);
        assert_eq!(
            cache.lookup(&identity, &host, &settings),
            hit.then(|| answer(version))
        );
        assert_eq!(cache.hits(), u64::from(hit));
        if !hit {
            record_branch(&store, &identity, &host)?;
        }
    }

    host.version.set(3);
    let store = crate::store::Store::open(&dir)?;
    assert_eq!(
        ResultCache::persistent(&store).lookup(&identity, &host, &settings),
        None
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn reopened_cache_reuses_reads_of_a_previous_copied_path() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "copy-revert");
    let identity = identity(b"copy-revert");
    let host = HistoryHost::new(1);
    let settings = crate::eval::Settings::default();
    let mut shapes = Vec::new();

    for (version, hit) in [(1, false), (1, true), (2, false), (2, true), (1, true)] {
        host.version.set(version);
        let store = crate::store::Store::open(&dir)?;
        let mut cache = ResultCache::persistent(&store);
        assert_eq!(
            cache.lookup(&identity, &host, &settings),
            hit.then(|| answer(version))
        );
        if !hit {
            let recorder = RecordingHost::new(&host);
            let copied = recorder
                .copy_to_store(&crate::value2::ambient_path("/input"))
                .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
            recorder.read_file(&crate::value2::ambient_path(copied))?;
            let reads = recorder.take();
            shapes.push(reads.questions());
            cache.record(&identity, &reads, &answer(version), &host, &settings)?;
        }
    }

    assert_eq!(shapes.len(), 2, "a historical hit was evaluated again");
    assert_ne!(
        shapes.first(),
        shapes.get(1),
        "the copied path did not affect later questions"
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn witness_history_retains_four_distinct_shapes() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "bounded");
    let identity = identity(b"bounded");
    let host = HistoryHost::new(1);
    let settings = crate::eval::Settings::default();
    assert_eq!(WITNESS_HISTORY, 4);

    for version in 1..=5 {
        host.version.set(version);
        record_branch(&crate::store::Store::open(&dir)?, &identity, &host)?;
    }

    let store = crate::store::Store::open(&dir)?;
    assert_eq!(std::fs::read_dir(store.witness_dir())?.count(), 8);
    for slot in 0..WITNESS_HISTORY {
        assert!(matches!(
            store.witness().get(&witness_slot(&identity, slot)),
            WitnessLookup::Found(_)
        ));
    }
    for version in 2..=5 {
        host.version.set(version);
        assert_eq!(
            ResultCache::persistent(&store).lookup(&identity, &host, &settings),
            Some(answer(version))
        );
    }
    host.version.set(1);
    assert_eq!(
        ResultCache::persistent(&store).lookup(&identity, &host, &settings),
        None
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn recording_a_known_shape_does_not_displace_other_shapes() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "deduplicated");
    let identity = identity(b"deduplicated");
    let host = HistoryHost::new(1);
    for version in [1, 2, 1, 1, 1, 1, 1, 1] {
        host.version.set(version);
        record_branch(&crate::store::Store::open(&dir)?, &identity, &host)?;
    }

    let store = crate::store::Store::open(&dir)?;
    assert_eq!(
        std::fs::read_dir(store.witness_dir())?.count(),
        4,
        "repeated shapes consumed history slots"
    );
    host.version.set(2);
    assert_eq!(
        ResultCache::persistent(&store).lookup(&identity, &host, &crate::eval::Settings::default()),
        Some(answer(2))
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn a_newest_hit_does_not_read_a_corrupt_older_witness() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "lazy");
    let identity = identity(b"lazy");
    let host = HistoryHost::new(1);
    for version in [1, 2] {
        host.version.set(version);
        record_branch(&crate::store::Store::open(&dir)?, &identity, &host)?;
    }

    let store = crate::store::Store::open(&dir)?;
    let older = store.witness().path(&witness_slot(&identity, 1));
    assert!(older.is_file(), "no older witness was persisted");
    std::fs::write(&older, b"deliberately malformed older witness")?;
    let mut cache = ResultCache::persistent(&store);
    assert_eq!(
        cache.lookup(&identity, &host, &crate::eval::Settings::default()),
        Some(answer(2))
    );
    assert!(
        cache.take_corruption().is_empty(),
        "newest hit decoded an irrelevant older witness"
    );
    assert_eq!(
        std::fs::read(older)?,
        b"deliberately malformed older witness"
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn forgetting_a_served_result_removes_every_witness_slot() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "forget");
    let identity = identity(b"forget");
    let host = HistoryHost::new(1);
    let settings = crate::eval::Settings::default();
    for version in 1..=4 {
        host.version.set(version);
        record_branch(&crate::store::Store::open(&dir)?, &identity, &host)?;
    }

    let store = crate::store::Store::open(&dir)?;
    let mut cache = ResultCache::persistent(&store);
    host.version.set(2);
    assert_eq!(cache.lookup(&identity, &host, &settings), Some(answer(2)));
    let key = cache
        .served_key()
        .ok_or("a hit did not expose its served key")?;
    cache.forget(&identity, key, "test consumer rejected the answer")?;
    assert_eq!(cache.served_key(), None);
    for slot in 0..WITNESS_HISTORY {
        let slot = witness_slot(&identity, slot);
        assert_eq!(store.witness().get(&slot), WitnessLookup::Missing);
        assert!(!store.witness().refs_path(&slot).exists());
    }
    for version in 1..=4 {
        host.version.set(version);
        assert_eq!(
            cache.lookup(&identity, &host, &settings),
            None,
            "in-memory history survived forget"
        );
        let reopened = crate::store::Store::open(&dir)?;
        assert_eq!(
            ResultCache::persistent(&reopened).lookup(&identity, &host, &settings),
            None,
            "persistent history survived forget"
        );
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn all_four_history_sidecars_keep_their_aterms_alive_during_a_sweep() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "gc-roots");
    let identity = identity(b"gc-roots");
    let host = HistoryHost::new(1);
    let settings = crate::eval::Settings::default();
    let mut aterms = Vec::new();
    for version in 1..=4 {
        host.version.set(version);
        let store = crate::store::Store::open(&dir)?;
        let recorder = RecordingHost::new(&host);
        recorder.read_file(&crate::value2::ambient_path("/selector"))?;
        let aterm = format!("Derive([], historical-{version})");
        // Even a refused derivation write records the ATerm it attempted.
        drop(recorder.write_derivation("historical", &aterm));
        aterms.push(ObjId::of(aterm.as_bytes()));
        ResultCache::persistent(&store).record(
            &identity,
            &recorder.take(),
            &answer(version),
            &host,
            &settings,
        )?;
    }

    let store = crate::store::Store::open(&dir)?;
    let orphan = store.cas().put(b"unreferenced sweep control")?;
    let report = store.sweep(u64::MAX)?;
    assert_eq!(report.witnesses_left, 4, "{report:?}");
    assert_eq!(report.witnesses_removed, 0, "{report:?}");
    assert_eq!(report.witnesses_unreadable, 0, "{report:?}");
    assert_eq!(report.objects_removed, 1, "{report:?}");
    assert!(
        !store.cas().has(orphan)?,
        "the control object was not swept"
    );
    for aterm in aterms {
        assert!(
            store.cas().has(aterm)?,
            "a historical ATerm lost its witness root"
        );
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

fn derivation_witness(aterm: ObjId) -> Vec<WitnessRow> {
    vec![(
        Question::WriteDrv {
            name: "history".to_owned(),
            answer: WriteDrvAnswer::Written("/nix/store/history.drv".to_owned()),
            aterm,
        },
        Recorded::Digest(Hash::from_bytes([0; 32])),
    )]
}

fn assert_surviving_aterms(store: &crate::store::Store, identities: &[EvalId]) -> TestResult {
    for identity in identities {
        if let WitnessLookup::Found(rows) = store.witness().get(identity) {
            for (question, _) in rows {
                if let Question::WriteDrv { aterm, .. } = question {
                    assert!(
                        store.cas().has(aterm)?,
                        "a surviving witness lost its ATerm during the sweep"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn interrupted_slot_moves_never_publish_mismatched_ownership() -> TestResult {
    for stop in 0..=3 {
        let dir = crate::eval::scratch_dir("ixe-history", &format!("move-interrupted-{stop}"));
        let store = crate::store::Store::open(&dir)?;
        let from = identity(b"move-source");
        let to = identity(b"move-destination");
        let guard = store.publication_guard()?;
        let source_aterm = store.cas().put(b"Derive([], move-source)")?;
        let replaced_aterm = store.cas().put(b"Derive([], move-destination)")?;
        let source = derivation_witness(source_aterm);
        store.witness().put(&from, &source)?;
        store
            .witness()
            .put(&to, &derivation_witness(replaced_aterm))?;

        let mut reached = None;
        let interrupted = store.witness().move_slot_with_hook(&from, &to, |step| {
            reached = Some(step);
            if step == stop {
                Err(std::io::Error::other("injected publication interruption"))
            } else {
                Ok(())
            }
        });
        assert!(interrupted.is_err());
        assert_eq!(
            reached,
            Some(stop),
            "the intended interruption was not reached"
        );
        let expected_from = if stop == 0 {
            WitnessLookup::Found(source.clone())
        } else {
            WitnessLookup::Missing
        };
        let expected_to = if stop == 3 {
            WitnessLookup::Found(source)
        } else {
            WitnessLookup::Missing
        };
        assert_eq!(store.witness().get(&from), expected_from);
        assert_eq!(store.witness().get(&to), expected_to);
        drop(guard);

        store.sweep(u64::MAX)?;
        assert_surviving_aterms(&store, &[from, to])?;
        assert_eq!(store.witness().get(&from), expected_from);
        assert_eq!(store.witness().get(&to), expected_to);
        assert_eq!(store.cas().has(source_aterm)?, stop == 0 || stop == 3);
        assert!(
            !store.cas().has(replaced_aterm)?,
            "a replaced destination kept an unowned ATerm"
        );
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[test]
fn interrupted_witness_replacement_never_pairs_old_body_with_new_refs() -> TestResult {
    for stop in 0..=2 {
        let dir = crate::eval::scratch_dir("ixe-history", &format!("put-interrupted-{stop}"));
        let store = crate::store::Store::open(&dir)?;
        let identity = identity(b"replace");
        let guard = store.publication_guard()?;
        let old_aterm = store.cas().put(b"Derive([], old-body)")?;
        let new_aterm = store.cas().put(b"Derive([], new-body)")?;
        store
            .witness()
            .put(&identity, &derivation_witness(old_aterm))?;
        let replacement = derivation_witness(new_aterm);

        let mut reached = None;
        let interrupted = store.witness().put_with_shape_hook(
            &identity,
            &replacement,
            witness_shape(&replacement),
            |step| {
                reached = Some(step);
                if step == stop {
                    Err(std::io::Error::other("injected publication interruption"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(interrupted.is_err());
        assert_eq!(
            reached,
            Some(stop),
            "the intended interruption was not reached"
        );
        let expected = if stop == 2 {
            WitnessLookup::Found(replacement)
        } else {
            WitnessLookup::Missing
        };
        assert_eq!(store.witness().get(&identity), expected);
        drop(guard);

        store.sweep(u64::MAX)?;
        assert_surviving_aterms(&store, &[identity])?;
        assert_eq!(store.witness().get(&identity), expected);
        assert!(
            !store.cas().has(old_aterm)?,
            "the invalidated old body retained its ATerm"
        );
        assert_eq!(store.cas().has(new_aterm)?, stop == 2);
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[test]
fn a_corrupt_newest_witness_falls_through_to_healthy_history() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "corrupt-newest");
    let identity = identity(b"corrupt-newest");
    let host = HistoryHost::new(1);
    let settings = crate::eval::Settings::default();
    for version in [1, 2] {
        host.version.set(version);
        record_branch(&crate::store::Store::open(&dir)?, &identity, &host)?;
    }

    let store = crate::store::Store::open(&dir)?;
    assert_eq!(
        ResultCache::persistent(&store).lookup(&identity, &host, &settings),
        Some(answer(2))
    );
    std::fs::write(store.witness().path(&identity), b"corrupt newest body")?;
    assert!(matches!(
        store.witness().get(&identity),
        WitnessLookup::Refused(_)
    ));
    host.version.set(1);
    let mut cache = ResultCache::persistent(&store);
    assert_eq!(cache.lookup(&identity, &host, &settings), Some(answer(1)));
    assert_eq!(cache.hits(), 1);
    assert!(
        !cache.take_corruption().is_empty(),
        "the corrupt newest candidate was silently ignored"
    );

    host.version.set(2);
    assert_eq!(
        ResultCache::persistent(&store).lookup(&identity, &host, &settings),
        None,
        "a corrupt candidate was served"
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn a_restored_history_hit_keeps_one_payload_and_reuses_it_without_decoding() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "preferred-reuse");
    let store = crate::store::Store::open(&dir)?;
    let identity = identity(b"preferred-reuse");
    let host = HistoryHost::new(1);
    let settings = crate::eval::Settings::default();
    let mut cache = ResultCache::persistent(&store);
    for (version, hit) in [(1, false), (1, true), (2, false), (2, true), (1, true)] {
        host.version.set(version);
        let wasted_before = cache.wasted_replays();
        assert_eq!(
            cache.lookup(&identity, &host, &settings),
            hit.then(|| answer(version))
        );
        if hit {
            assert_eq!(
                cache.wasted_replays(),
                wasted_before,
                "a recovered history hit counted as wasted evaluation"
            );
        } else {
            cache.note_miss(&identity);
            record_cached_branch(&mut cache, &identity, &host)?;
        }
    }
    assert_eq!(
        cache.wasted_replays(),
        1,
        "only the unseen B state required wasted replay"
    );
    let cached_slots: Vec<_> = (0..WITNESS_HISTORY)
        .map(|slot| witness_slot(&identity, slot))
        .filter(|slot| cache.witness.contains_key(slot))
        .collect();
    assert_eq!(
        cached_slots.len(),
        1,
        "persistent cache retained multiple decoded history payloads"
    );
    let cached = cached_slots
        .first()
        .ok_or("no successful witness was retained")?;
    std::fs::write(
        store.witness().path(cached),
        b"must not decode the cached successful recipe",
    )?;
    assert!(cache.take_corruption().is_empty());
    host.selector_reads.set(0);

    assert_eq!(cache.lookup(&identity, &host, &settings), Some(answer(1)));
    assert_eq!(
        host.selector_reads.get(),
        1,
        "repeated A replayed another history candidate"
    );
    assert!(
        cache.take_corruption().is_empty(),
        "repeated A decoded its corrupted disk body"
    );
    assert_eq!(cache.wasted_replays(), 1);
    assert_eq!(cache.witness.len(), 1);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn a_missing_result_object_counts_one_wasted_replay_when_the_miss_is_noted() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "object-miss-count");
    let store = crate::store::Store::open(&dir)?;
    let identity = identity(b"object-miss-count");
    let host = HistoryHost::new(1);
    let settings = crate::eval::Settings::default();
    let mut cache = ResultCache::persistent(&store);
    record_cached_branch(&mut cache, &identity, &host)?;
    assert_eq!(cache.lookup(&identity, &host, &settings), Some(answer(1)));
    let key = cache.served_key().ok_or("control hit has no served key")?;
    let output = cache
        .table
        .get(eval_domain(), key)
        .ok_or("control row absent from memory")?
        .output;
    std::fs::remove_file(store.objects_dir().join(output.hash().to_hex()))?;

    assert_eq!(cache.lookup(&identity, &host, &settings), None);
    assert!(
        !cache.take_corruption().is_empty(),
        "missing object did not reach the failure path"
    );
    assert_eq!(
        cache.wasted_replays(),
        0,
        "lookup counted a miss before the consumer reported it"
    );
    cache.note_miss(&identity);
    assert_eq!(
        cache.wasted_replays(),
        1,
        "object failure was lost or counted twice"
    );
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn another_process_rotating_history_does_not_hide_the_replaced_cached_slot() -> TestResult {
    for rotations in [1, 2] {
        let dir =
            crate::eval::scratch_dir("ixe-history", &format!("external-rotation-{rotations}"));
        let store = crate::store::Store::open(&dir)?;
        let identity = identity(b"external-rotation");
        let host = HistoryHost::new(1);
        let settings = crate::eval::Settings::default();
        let mut first_process = ResultCache::persistent(&store);
        record_cached_branch(&mut first_process, &identity, &host)?;
        assert_eq!(
            first_process.lookup(&identity, &host, &settings),
            Some(answer(1))
        );
        assert!(first_process.witness.contains_key(&identity));

        for version in 2..=(rotations + 1) {
            host.version.set(version);
            let other_store = crate::store::Store::open(&dir)?;
            record_branch(&other_store, &identity, &host)?;
        }
        host.version.set(2);
        assert_eq!(
            first_process.lookup(&identity, &host, &settings),
            Some(answer(2)),
            "cached A hid B after another process replaced slot zero or rotated B to slot one"
        );
        assert_eq!(first_process.wasted_replays(), 0);
        assert!(first_process.take_corruption().is_empty());
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[test]
fn recipe_fingerprint_includes_retained_derivation_answers() -> TestResult {
    let dir = crate::eval::scratch_dir("ixe-history", "retained-answer");
    let store = crate::store::Store::open(&dir)?;
    let identity = identity(b"retained-answer");
    let aterm = store.cas().put(b"Derive([], retained-answer)")?;
    let original = derivation_witness(aterm);
    store.witness().put(&identity, &original)?;
    let original_shape = witness_shape(&original);
    let mut distinct_shapes = std::collections::BTreeSet::from([original_shape]);
    for answer in [
        WriteDrvAnswer::Written("/nix/store/another.drv".to_owned()),
        WriteDrvAnswer::Failed {
            outcome: "failed".to_owned(),
            detail: "first".to_owned(),
        },
        WriteDrvAnswer::Failed {
            outcome: "failed".to_owned(),
            detail: "second".to_owned(),
        },
        WriteDrvAnswer::Failed {
            outcome: "unavailable".to_owned(),
            detail: "first".to_owned(),
        },
    ] {
        let mut changed = original.clone();
        let Some((
            Question::WriteDrv {
                answer: retained, ..
            },
            _,
        )) = changed.first_mut()
        else {
            return Err("fixture did not contain a derivation write".into());
        };
        *retained = answer;
        assert!(
            distinct_shapes.insert(witness_shape(&changed)),
            "a retained answer field was omitted"
        );
        // Keep the original recorded digest and ownership metadata. Altering
        // this extra replay field must still invalidate the persisted body.
        let value = CanonValue::map([
            ("format", CanonValue::str(WITNESS_FORMAT)),
            (
                "rows",
                CanonValue::Array(changed.iter().map(row_value).collect()),
            ),
        ]);
        let bytes = canon::encode(&value)?;
        assert_eq!(witness_rows(&bytes), Some(changed));
        std::fs::write(store.witness().path(&identity), bytes)?;
        assert!(matches!(
            store.witness().get(&identity),
            WitnessLookup::Refused(_)
        ));
    }
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
