use super::tests::test_server_state;
use super::*;

fn matched_paths(response: &str) -> std::collections::BTreeSet<String> {
    let response: serde_json::Value = serde_json::from_str(response).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    response["result"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["file"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn filename_preparation_allows_queries_and_publishes_visibility_with_membership() {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let index_dir = root.join("custom-index");
    let state = test_server_state(&root, &index_dir);
    for path in ["disk.txt", "removed.txt"] {
        std::fs::write(root.join(path), "needle\n").unwrap();
    }
    builder::build_index(&root, Some(&index_dir), false, false, &[]).unwrap();
    {
        let mut index = state.index.write().unwrap();
        *index = HybridIndex::open(&index_dir, &root).unwrap();
        index.live.upsert_file("overlay.txt", b"needle\n");
        index.live.delete_file("removed.txt");
    }
    state
        .filename_extra_paths
        .write()
        .unwrap()
        .insert("old.bin".into());
    state.filename_index_ready.store(true, Ordering::SeqCst);
    let weak = Arc::downgrade(&state);
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_observed = Arc::clone(&observed);
    *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::PreparingFilenamePaths) {
            let state = weak.upgrade().unwrap();
            let reader = state
                .index
                .try_read()
                .expect("filename preparation excluded queries");
            drop(reader);
            let response: serde_json::Value =
                serde_json::from_str(&handle_files(None, &serde_json::Value::Null, &state))
                    .unwrap();
            assert_eq!(
                response["result"]["files"],
                serde_json::json!(["disk.txt", "old.bin", "overlay.txt"])
            );
            hook_observed.store(true, Ordering::SeqCst);
        }
    }));
    let listed: Vec<String> = [
        "disk.txt",
        "overlay.txt",
        "removed.txt",
        ".new.bin",
        "plain.bin",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let mut visibility = tgrep_core::visibility::PathVisibility::default();
    visibility.record(".new.bin", false, None, None);
    let _gate = state.snapshot_gate.write().unwrap();
    assert!(replace_filename_extra_paths(&state, &listed, &visibility));
    *state.stale_refresh_hook.lock().unwrap() = None;
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(
        *state.filename_extra_paths.read().unwrap(),
        std::collections::HashSet::from([
            "removed.txt".into(),
            ".new.bin".into(),
            "plain.bin".into()
        ])
    );
    for (hidden, expected) in [
        (
            false,
            serde_json::json!(["disk.txt", "overlay.txt", "plain.bin", "removed.txt"]),
        ),
        (
            true,
            serde_json::json!([
                ".new.bin",
                "disk.txt",
                "overlay.txt",
                "plain.bin",
                "removed.txt"
            ]),
        ),
    ] {
        let response: serde_json::Value = serde_json::from_str(&handle_files(
            None,
            &serde_json::json!({"hidden": hidden}),
            &state,
        ))
        .unwrap();
        assert_eq!(response["result"]["files"], expected);
    }
    state.filename_index_dirty.store(false, Ordering::SeqCst);
    assert!(!replace_filename_extra_paths(&state, &listed, &visibility));
    assert!(!state.filename_index_dirty.load(Ordering::SeqCst));
}

#[test]
fn legacy_hidden_coverage_is_published_only_after_reconciliation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let index_dir = root.join("custom-index");
    std::fs::create_dir_all(root.join(".github/.nested")).unwrap();
    for path in [
        "visible.txt",
        ".secret.txt",
        ".github/settings.txt",
        ".github/.nested/inner.txt",
    ] {
        std::fs::write(root.join(path), "needle\n").unwrap();
    }
    let state = test_server_state(&root, &index_dir);
    builder::build_index(&root, Some(&index_dir), false, false, &[]).unwrap();
    *state.index.write().unwrap() = HybridIndex::open(&index_dir, &root).unwrap();
    state.hidden_complete.store(false, Ordering::SeqCst);
    assert_eq!(state.index.read().unwrap().num_files(), 1);
    let weak = Arc::downgrade(&state);
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_observed = Arc::clone(&observed);
    *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterMatcherPublish) {
            let state = weak.upgrade().unwrap();
            assert!(!state.hidden_complete.load(Ordering::SeqCst));
            assert!(
                !tgrep_core::meta::IndexMeta::load(&state.index_dir)
                    .unwrap()
                    .hidden_complete
            );
            let response = handle_search(
                None,
                &serde_json::json!({"pattern": "needle", "hidden": true}),
                &state,
            );
            assert!(
                serde_json::from_str::<serde_json::Value>(&response)
                    .unwrap()
                    .get("error")
                    .is_some()
            );
            hook_observed.store(true, Ordering::SeqCst);
        }
    }));
    assert!(background_refresh_stale(&state, &root, &index_dir, false));
    *state.stale_refresh_hook.lock().unwrap() = None;
    assert!(observed.load(Ordering::SeqCst));
    assert!(state.hidden_complete.load(Ordering::SeqCst));
    assert!(
        tgrep_core::meta::IndexMeta::load(&index_dir)
            .unwrap()
            .hidden_complete
    );

    // No hidden parameter is how older clients request ordinary visibility.
    assert_eq!(
        matched_paths(&handle_search(
            None,
            &serde_json::json!({"pattern": "needle", "max_count": 1}),
            &state
        )),
        std::collections::BTreeSet::from(["visible.txt".to_string()])
    );
    assert_eq!(
        matched_paths(&handle_search(
            None,
            &serde_json::json!({"pattern": "needle", "hidden": true}),
            &state
        ))
        .len(),
        4
    );
    assert_eq!(
        matched_paths(&handle_search(
            None,
            &serde_json::json!({"pattern": "needle", "scope": ".github/"}),
            &state
        )),
        std::collections::BTreeSet::from([".github/settings.txt".to_string()])
    );
    let response: serde_json::Value =
        serde_json::from_str(&handle_files(None, &serde_json::Value::Null, &state)).unwrap();
    assert_eq!(
        response["result"]["files"],
        serde_json::json!(["visible.txt"])
    );

    state.indexing.store(true, Ordering::SeqCst);
    for response in [
        handle_search(
            None,
            &serde_json::json!({"pattern": "needle", "hidden": true}),
            &state,
        ),
        handle_files(None, &serde_json::json!({"hidden": true}), &state),
    ] {
        assert!(
            serde_json::from_str::<serde_json::Value>(&response)
                .unwrap()
                .get("error")
                .is_some()
        );
    }
}

#[test]
fn custom_storage_events_never_enter_the_overlay_or_deferred_queue() {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let index_dir = root.join("custom-index");
    let state = test_server_state(&root, &index_dir);
    let path = index_dir.join("output.txt");
    std::fs::write(&path, "needle\n").unwrap();
    assert!(
        watchable_dirs(&root, &index_dir, &[], None, &index_dir)
            .dirs
            .is_empty()
    );
    reindex_file(&state, &path, "custom-index/output.txt", true);
    assert!(
        !state
            .index
            .read()
            .unwrap()
            .has_active_path("custom-index/output.txt")
    );
    state.indexing.store(true, Ordering::SeqCst);
    let event = Event {
        kind: EventKind::Create(notify::event::CreateKind::File),
        paths: vec![path],
        attrs: Default::default(),
    };
    assert!(defer_events_during_build(&state, &event));
    assert!(
        state
            .deferred_events
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reload_preserves_hidden_coverage_visibility_and_storage_exclusion() {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let index_dir = root.join("custom-index");
    let state = test_server_state(&root, &index_dir);
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(root.join(".hidden/.ignore"), "ignored.txt\n").unwrap();
    for path in [
        "visible.txt",
        ".secret.txt",
        ".hidden/open.txt",
        ".hidden/ignored.txt",
        "custom-index/output.txt",
    ] {
        std::fs::write(root.join(path), "needle\n").unwrap();
    }

    for _ in 0..2 {
        let response = handle_reload(None, &state);
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(response.get("error").is_none(), "{response}");
        let meta = tgrep_core::meta::IndexMeta::load(&index_dir).unwrap();
        assert!(meta.complete && meta.hidden_complete);
        assert_eq!(
            matched_paths(&handle_search(
                None,
                &serde_json::json!({"pattern": "needle"}),
                &state
            )),
            std::collections::BTreeSet::from(["visible.txt".to_string()])
        );
        assert_eq!(
            matched_paths(&handle_search(
                None,
                &serde_json::json!({"pattern": "needle", "hidden": true}),
                &state
            )),
            std::collections::BTreeSet::from([
                "visible.txt".to_string(),
                ".secret.txt".to_string(),
                ".hidden/open.txt".to_string(),
            ])
        );
    }
}

#[test]
fn hidden_filename_publication_is_coherent_even_when_metadata_publish_fails() {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let index_dir = root.join("custom-index");
    let state = test_server_state(&root, &index_dir);
    std::fs::write(root.join("visible.txt"), "needle\n").unwrap();
    assert!(background_refresh_stale(&state, &root, &index_dir, false));
    std::fs::write(root.join(".secret.png"), "needle\n").unwrap();
    std::fs::write(root.join("visible.png"), "needle\n").unwrap();

    let weak = Arc::downgrade(&state);
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_observed = Arc::clone(&observed);
    *state.stale_refresh_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(phase, StaleRefreshPhase::AfterFilenameSidecarPublish) {
            let state = weak.upgrade().unwrap();
            let response: serde_json::Value =
                serde_json::from_str(&handle_files(None, &serde_json::Value::Null, &state))
                    .unwrap();
            assert_eq!(
                response["result"]["files"],
                serde_json::json!(["visible.png", "visible.txt"])
            );
            let sidecar = tgrep_core::path_index::read_filename_index(&state.index_dir)
                .unwrap()
                .unwrap();
            assert!(sidecar.paths.contains(&".secret.png".to_string()));
            let visibility = sidecar.visibility.unwrap();
            assert!(!visibility.paths.is_visible(".secret.png", "", false));
            let old_meta = tgrep_core::meta::IndexMeta::load(&state.index_dir).unwrap();
            assert!(old_meta.hidden_complete);
            assert!(old_meta.visibility.is_visible(".secret.png", "", false));
            assert_eq!(old_meta.file_table_id, Some(visibility.file_table_id));
            // Deterministically interrupt the second publication. The already
            // published sidecar must remain safe with the old metadata.
            std::fs::remove_file(
                state
                    .index_dir
                    .join(".filename-index-staging")
                    .join("meta.json"),
            )
            .unwrap();
            hook_observed.store(true, Ordering::SeqCst);
        }
    }));
    assert!(!background_refresh_stale(&state, &root, &index_dir, false));
    assert!(observed.load(Ordering::SeqCst));
    assert!(state.filename_index_dirty.load(Ordering::SeqCst));
    let reader = tgrep_core::reader::IndexReader::open(&index_dir).unwrap();
    let old_meta = tgrep_core::meta::IndexMeta::load(&index_dir).unwrap();
    let restarted =
        StartupDiscovery::load(&index_dir, Some(old_meta.clone()), reader.file_table_id());
    assert!(restarted.hidden_complete && restarted.filename_index_ready);
    let mut visible: Vec<_> = reader
        .all_paths()
        .iter()
        .cloned()
        .chain(restarted.filename_extra_paths)
        .filter(|path| restarted.visibility.is_visible(path, "", false))
        .collect();
    visible.sort();
    assert_eq!(visible, ["visible.png", "visible.txt"]);
    let incomplete_sidecar = index_dir.join("incomplete-sidecar");
    tgrep_core::path_index::write_extra_paths_with_visibility(
        &incomplete_sidecar,
        &[".secret.png".to_string()],
        &Default::default(),
        reader.file_table_id(),
        false,
    )
    .unwrap();
    let incomplete = StartupDiscovery::load(
        &incomplete_sidecar,
        Some(old_meta.clone()),
        reader.file_table_id(),
    );
    assert!(!incomplete.hidden_complete && incomplete.filename_index_ready);

    let mut mismatched_meta = old_meta.clone();
    mismatched_meta.file_table_id = Some([0; 32]);
    assert!(
        !StartupDiscovery::load(&index_dir, Some(mismatched_meta), reader.file_table_id())
            .hidden_complete
    );
    let bad_sidecar = index_dir.join("bad-sidecar");
    tgrep_core::path_index::write_extra_paths_with_visibility(
        &bad_sidecar,
        &[".secret.png".to_string()],
        &Default::default(),
        [0; 32],
        true,
    )
    .unwrap();
    let mismatched = StartupDiscovery::load(&bad_sidecar, Some(old_meta), reader.file_table_id());
    assert!(!mismatched.hidden_complete && !mismatched.filename_index_ready);
    *state.stale_refresh_hook.lock().unwrap() = None;
    assert!(background_refresh_stale(&state, &root, &index_dir, false));
    assert!(!state.filename_index_dirty.load(Ordering::SeqCst));
    assert!(
        !tgrep_core::meta::IndexMeta::load(&index_dir)
            .unwrap()
            .visibility
            .is_visible(".secret.png", "", false)
    );
}

#[test]
fn native_hidden_directory_rename_schedules_descendant_reconciliation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let index_dir = root.join("custom-index");
    let state = test_server_state(&root, &index_dir);
    std::fs::create_dir(root.join(".moving")).unwrap();
    std::fs::write(root.join(".moving/child.txt"), "needle\n").unwrap();
    std::fs::write(root.join(".moving/.asset.png"), "needle\n").unwrap();
    assert!(background_refresh_stale(&state, &root, &index_dir, false));
    state.ignore_refresh_scheduled.store(true, Ordering::SeqCst);
    std::fs::rename(root.join(".moving"), root.join("moved")).unwrap();
    handle_fs_event(
        &state,
        &root,
        &Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::From,
        )))
        .add_path(root.join(".moving")),
    );
    assert!(state.ignore_rules_dirty.load(Ordering::SeqCst));
    assert!(background_refresh_stale(&state, &root, &index_dir, false));
    assert_eq!(
        matched_paths(&handle_search(
            None,
            &serde_json::json!({"pattern":"needle"}),
            &state
        )),
        std::collections::BTreeSet::from(["moved/child.txt".to_string()])
    );
    let response: serde_json::Value = serde_json::from_str(&handle_files(
        None,
        &serde_json::json!({"hidden":true}),
        &state,
    ))
    .unwrap();
    assert_eq!(
        response["result"]["files"],
        serde_json::json!(["moved/.asset.png", "moved/child.txt"])
    );
}

#[test]
fn filename_only_legacy_upgrade_publishes_the_core_format_boundary() {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let index_dir = root.join("custom-index");
    let state = test_server_state(&root, &index_dir);
    std::fs::write(root.join("visible.png"), "needle\n").unwrap();
    std::fs::write(root.join(".secret.png"), "needle\n").unwrap();
    let mut meta = tgrep_core::meta::IndexMeta::new(&root.to_string_lossy(), 0, 0);
    meta.version = 2;
    meta.save(&index_dir).unwrap();
    state.hidden_complete.store(false, Ordering::SeqCst);
    assert!(
        std::fs::read(index_dir.join("files.bin"))
            .unwrap()
            .is_empty()
    );

    assert!(background_refresh_stale(&state, &root, &index_dir, false));
    assert!(
        !std::fs::read(index_dir.join("files.bin"))
            .unwrap()
            .is_empty()
    );
    let meta = tgrep_core::meta::IndexMeta::load(&index_dir).unwrap();
    assert_eq!(meta.version, tgrep_core::meta::INDEX_FORMAT_VERSION);
    assert!(meta.hidden_complete);
    let reader = tgrep_core::reader::IndexReader::open(&index_dir).unwrap();
    assert_eq!(reader.num_files(), 0);
    assert_eq!(meta.file_table_id, Some(reader.file_table_id()));
    let response: serde_json::Value =
        serde_json::from_str(&handle_files(None, &serde_json::Value::Null, &state)).unwrap();
    assert_eq!(
        response["result"]["files"],
        serde_json::json!(["visible.png"])
    );
}
