use super::handle_tests::{Sess, take_c_string};
use super::warm_starts::{CacheDir, SettingsPin};
use super::*;

type TestResult = Result<(), Box<dyn core::error::Error>>;

struct CacheOwner(*mut IxeEvalCache);

impl CacheOwner {
    fn new(bytes: u64, entries: usize) -> Self {
        let pointer = ixe_eval_cache_new(bytes, entries);
        assert!(!pointer.is_null());
        Self(pointer)
    }

    fn stats(&self) -> IxeEvalCacheStats {
        let mut stats = IxeEvalCacheStats::default();
        // SAFETY: this owner keeps the handle live; stats is writable.
        assert_eq!(unsafe { ixe_eval_cache_stats(self.0, &mut stats) }, IXE_OK);
        stats
    }

    fn attach(&self, session: &Sess) {
        // SAFETY: both RAII owners keep their pointers live for the call.
        assert_eq!(
            unsafe { ixe_session_set_eval_cache(session.raw(), self.0) },
            IXE_OK
        );
    }
}

impl Drop for CacheOwner {
    fn drop(&mut self) {
        // SAFETY: this owner frees the handle exactly once.
        unsafe { ixe_eval_cache_free(self.0) };
    }
}

struct Started {
    mode: i32,
    root: u64,
    served: Option<String>,
}

struct Answer {
    text: String,
    mode: i32,
}

fn begin(session: &Sess, source: &str) -> Started {
    let base = ".";
    let path = IxeBytes {
        text: b"a".as_ptr(),
        len: 1,
    };
    let mut mode = -1;
    let mut root = 0;
    let mut answer = std::ptr::null_mut();
    // SAFETY: all input bytes and outputs live through the call; session owns
    // its pointer and any value handles issued through it.
    let status = unsafe {
        ixe_session_eval_question(
            session.raw(),
            source.as_ptr(),
            source.len(),
            base.as_ptr(),
            base.len(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            IXE_QUESTION_SELECT,
            &path,
            1,
            1,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            0,
            IXE_RENDER_RAW,
            &mut mode,
            &mut root,
            &mut answer,
        )
    };
    let served = take_c_string(answer);
    assert_eq!(status, IXE_OK, "question failed: {:?}", session.error());
    Started { mode, root, served }
}

fn finish(session: &Sess, started: Started) -> Answer {
    if started.mode == IXE_SERVE_ANSWER {
        assert!(started.served.is_some(), "served mode omitted its answer");
        return Answer {
            text: started.served.unwrap_or_default(),
            mode: started.mode,
        };
    }
    assert_eq!(
        started.mode, IXE_SERVE_EVALUATE,
        "fixture must evaluate or serve"
    );
    assert!(started.served.is_none());
    let mut selected = 0;
    let mut rendered = std::ptr::null_mut();
    // SAFETY: all handles belong to the still-live session, and outputs are
    // writable. Render transfers one owned string to take_c_string below.
    unsafe {
        assert_eq!(
            ixe_attrs_select(session.raw(), started.root, b"a".as_ptr(), 1, &mut selected),
            IXE_OK
        );
        assert_eq!(
            ixe_force(session.raw(), selected),
            IXE_OK,
            "force failed: {:?}",
            session.error()
        );
        assert_eq!(
            ixe_render(session.raw(), selected, IXE_RENDER_RAW, &mut rendered),
            IXE_OK
        );
    }
    let rendered = take_c_string(rendered);
    assert!(rendered.is_some(), "render omitted its string");
    let text = rendered.unwrap_or_default();
    // SAFETY: the answer bytes remain live for this terminal protocol call.
    assert_eq!(
        unsafe { ixe_session_question_answer(session.raw(), IXE_OK, text.as_ptr(), text.len()) },
        IXE_OK
    );
    Answer {
        text,
        mode: started.mode,
    }
}

fn ask(owner: &CacheOwner, source: &str) -> Answer {
    let session = Sess::new();
    owner.attach(&session);
    let started = begin(&session, source);
    finish(&session, started)
}

#[test]
fn fresh_capi_sessions_reuse_decoded_history_across_edits_and_reverts() -> TestResult {
    let _pin = SettingsPin::exclusive();
    let dir = crate::eval::scratch_dir("ixe-capi-retained", "edit-revert");
    std::fs::create_dir_all(&dir)?;
    let _cache = CacheDir::set(&dir.join("cache"));
    let selector = dir.join("selector");
    let branch_a = dir.join("branch-a");
    let branch_b = dir.join("branch-b");
    std::fs::write(&branch_a, "answer-A")?;
    std::fs::write(&branch_b, "answer-B")?;
    let source = format!(
        "let state = builtins.readFile {}; in {{ a = if state == \"A\" then builtins.readFile {} else builtins.readFile {}; }}",
        serde_json::to_string(&selector.to_string_lossy())?,
        serde_json::to_string(&branch_a.to_string_lossy())?,
        serde_json::to_string(&branch_b.to_string_lossy())?,
    );
    let owner = CacheOwner::new(8 * 1024 * 1024, 4);
    let mut previous = owner.stats();

    for (state, expected, mode, repeats) in [
        ("A", "answer-A", IXE_SERVE_EVALUATE, false),
        ("A", "answer-A", IXE_SERVE_ANSWER, true),
        ("B", "answer-B", IXE_SERVE_EVALUATE, false),
        ("B", "answer-B", IXE_SERVE_ANSWER, true),
        ("A", "answer-A", IXE_SERVE_ANSWER, false),
        ("A", "answer-A", IXE_SERVE_ANSWER, true),
    ] {
        std::fs::write(&selector, state)?;
        let answer = ask(&owner, &source);
        assert_eq!(answer.text, expected);
        assert_eq!(answer.mode, mode, "wrong reuse decision for state {state}");
        let now = owner.stats();
        if repeats {
            assert!(
                now.memory_hits > previous.memory_hits,
                "repeat did not use the explicit shared owner"
            );
            assert_eq!(
                now.disk_loads, previous.disk_loads,
                "repeat decoded a witness from disk"
            );
        }
        assert_eq!(
            now.entries, 1,
            "one evaluation retained more than its preferred recipe"
        );
        assert!(now.retained_bytes > 0 && now.retained_bytes <= 8 * 1024 * 1024);
        previous = now;
    }
    // Cache directories remain: other tests can have opened this global store.
    std::fs::remove_file(selector)?;
    std::fs::remove_file(branch_a)?;
    std::fs::remove_file(branch_b)?;
    Ok(())
}

#[test]
fn attached_sessions_keep_the_owner_alive_after_its_c_handle_is_freed() -> TestResult {
    let _pin = SettingsPin::exclusive();
    let dir = crate::eval::scratch_dir("ixe-capi-retained", "owner-lifetime");
    let _cache = CacheDir::set(&dir);
    let owner = CacheOwner::new(8 * 1024 * 1024, 4);
    let first = Sess::new();
    let second = Sess::new();
    owner.attach(&first);
    owner.attach(&second);
    drop(owner);

    let source = "{ a = \"alive after owner free\"; }";
    let cold = finish(&first, begin(&first, source));
    assert_eq!(cold.mode, IXE_SERVE_EVALUATE);
    drop(first);
    let mut corrupted = 0;
    for entry in std::fs::read_dir(dir.join("witness"))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            std::fs::write(
                entry.path(),
                b"disk witness unavailable after owner release",
            )?;
            corrupted += 1;
        }
    }
    assert!(
        corrupted > 0,
        "the disk-corruption control found no witness"
    );
    let warm = finish(&second, begin(&second, source));
    assert_eq!(warm.mode, IXE_SERVE_ANSWER);
    assert_eq!(warm.text, cold.text);
    Ok(())
}

#[test]
fn a_live_question_refuses_replacement_and_detachment_until_it_finishes() {
    let _pin = SettingsPin::exclusive();
    let dir = crate::eval::scratch_dir("ixe-capi-retained", "reattach");
    let _cache = CacheDir::set(&dir);
    let owner = CacheOwner::new(8 * 1024 * 1024, 4);
    let replacement = CacheOwner::new(8 * 1024 * 1024, 4);
    let session = Sess::new();
    owner.attach(&session);
    let started = begin(&session, "{ a = \"question still open\"; }");
    assert_eq!(started.mode, IXE_SERVE_EVALUATE);
    // SAFETY: all pointers remain live; null is the documented detach request.
    unsafe {
        assert_eq!(
            ixe_session_set_eval_cache(session.raw(), replacement.0),
            IXE_ERR_BADCALL
        );
        assert_eq!(
            ixe_session_set_eval_cache(session.raw(), std::ptr::null()),
            IXE_ERR_BADCALL
        );
    }
    assert_eq!(finish(&session, started).text, "question still open");
    assert_eq!(
        owner.stats().entries,
        1,
        "rejected reattachment changed the question's owner"
    );
    assert_eq!(replacement.stats().entries, 0);
    replacement.attach(&session);
    // SAFETY: session remains live and idle.
    assert_eq!(
        unsafe { ixe_session_set_eval_cache(session.raw(), std::ptr::null()) },
        IXE_OK
    );
}

#[test]
fn null_cache_calls_fail_with_defined_outputs_and_null_detaches() {
    let _pin = SettingsPin::exclusive();
    let owner = CacheOwner::new(1024, 1);
    let mut stats = IxeEvalCacheStats {
        memory_hits: 9,
        disk_loads: 9,
        retained_bytes: 9,
        entries: 9,
        evictions: 9,
    };
    // SAFETY: output is writable; null cases are explicit API inputs.
    unsafe {
        assert_eq!(
            ixe_eval_cache_stats(std::ptr::null(), &mut stats),
            IXE_ERR_BADCALL
        );
        assert_eq!(stats, IxeEvalCacheStats::default());
        assert_eq!(
            ixe_eval_cache_stats(owner.0, std::ptr::null_mut()),
            IXE_ERR_BADCALL
        );
        assert_eq!(
            ixe_session_set_eval_cache(std::ptr::null_mut(), owner.0),
            IXE_ERR_BADCALL
        );
        ixe_eval_cache_free(std::ptr::null_mut());
    }
    let session = Sess::new();
    owner.attach(&session);
    // SAFETY: session is live and idle; null detaches.
    assert_eq!(
        unsafe { ixe_session_set_eval_cache(session.raw(), std::ptr::null()) },
        IXE_OK
    );
    assert_eq!(owner.stats(), IxeEvalCacheStats::default());
}

#[test]
fn zero_retention_budgets_keep_disk_reuse_but_never_keep_decoded_payloads() {
    let _pin = SettingsPin::exclusive();
    for (bytes, entries) in [(0, 4), (8 * 1024 * 1024, 0)] {
        let dir = crate::eval::scratch_dir("ixe-capi-retained", "zero-budget");
        let _cache = CacheDir::set(&dir);
        let owner = CacheOwner::new(bytes, entries);
        let source = "{ a = \"disk reuse with zero retention\"; }";
        assert_eq!(ask(&owner, source).mode, IXE_SERVE_EVALUATE);
        let before = owner.stats();
        let warm = ask(&owner, source);
        assert_eq!(warm.mode, IXE_SERVE_ANSWER);
        assert_eq!(warm.text, "disk reuse with zero retention");
        let after = owner.stats();
        assert!(
            after.disk_loads > before.disk_loads,
            "zero-budget control did not load a disk witness"
        );
        assert_eq!(after.memory_hits, 0);
        assert_eq!(after.entries, 0);
        assert_eq!(after.retained_bytes, 0);
    }
}

#[test]
fn the_capi_entry_limit_evicts_and_reloads_then_reuses_the_loaded_recipe() {
    let _pin = SettingsPin::exclusive();
    let dir = crate::eval::scratch_dir("ixe-capi-retained", "entry-limit");
    let _cache = CacheDir::set(&dir);
    let owner = CacheOwner::new(8 * 1024 * 1024, 1);
    let first = "{ a = \"first retained evaluation\"; }";
    let second = "{ a = \"second retained evaluation\"; }";
    assert_eq!(ask(&owner, first).mode, IXE_SERVE_EVALUATE);
    assert_eq!(ask(&owner, second).mode, IXE_SERVE_EVALUATE);
    let displaced = owner.stats();
    assert_eq!(displaced.entries, 1);
    assert!(displaced.evictions >= 1);
    assert_eq!(ask(&owner, first).mode, IXE_SERVE_ANSWER);
    let reloaded = owner.stats();
    assert!(reloaded.disk_loads > displaced.disk_loads);
    assert_eq!(reloaded.entries, 1);
    assert_eq!(ask(&owner, first).mode, IXE_SERVE_ANSWER);
    let reused = owner.stats();
    assert!(reused.memory_hits > reloaded.memory_hits);
    assert_eq!(reused.disk_loads, reloaded.disk_loads);
    assert_eq!(reused.entries, 1);
}
