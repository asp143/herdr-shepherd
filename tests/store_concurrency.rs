use herdr_shepherd::store::{Store, Ticket};

#[test]
fn missing_store_read_does_not_create_state_directory() {
    let project = tempfile::tempdir().unwrap();
    let state = project.path().join("missing-state");
    let store = Store::open(&state, project.path()).unwrap();
    assert!(store.read().unwrap().tickets.is_empty());
    assert!(!state.exists());
    store
        .update(|data| {
            data.pending_pings.push("created by writer".into());
            Ok(())
        })
        .unwrap();
    assert_eq!(store.read().unwrap().pending_pings, ["created by writer"]);
}

#[test]
fn read_can_share_an_existing_read_lock() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), dir.path()).unwrap();
    store.update(|_| Ok(())).unwrap();
    let lock = std::fs::File::open(store.root().join("state.lock")).unwrap();
    lock.lock_shared().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || sender.send(store.read()).unwrap());
    let result = receiver.recv_timeout(std::time::Duration::from_secs(2));
    drop(lock);
    reader.join().unwrap();
    assert!(
        result
            .expect("read must share the held read lock")
            .unwrap()
            .tickets
            .is_empty()
    );
}

#[test]
fn roundtrip_store() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let store = Store::open(dir.path(), &project).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "SYM-1".into(),
                title: "Keep state".into(),
                ..Ticket::default()
            });
            data.pending_pings.push("SYM-1: done".into());
            Ok(())
        })
        .unwrap();
    let reopened = Store::open(dir.path(), &project).unwrap().read().unwrap();
    assert_eq!(reopened.schema_version, 2);
    assert_eq!(reopened.project_root, project);
    assert_eq!(reopened.tickets[0].id, "SYM-1");
    assert_eq!(reopened.tickets[0].title, "Keep state");
    assert_eq!(reopened.pending_pings, ["SYM-1: done"]);
}

#[test]
fn atomic_write_survives_partial() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), dir.path()).unwrap();
    store
        .update(|data| {
            data.pending_pings.push("committed".into());
            Ok(())
        })
        .unwrap();
    std::fs::write(store.root().join("state.json.tmp"), "{\"tickets\": [").unwrap();
    assert_eq!(store.read().unwrap().pending_pings, ["committed"]);
    store
        .update(|data| {
            data.pending_pings.push("next".into());
            Ok(())
        })
        .unwrap();
    assert_eq!(store.read().unwrap().pending_pings, ["committed", "next"]);
}

#[test]
fn two_threads_update_no_lost_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), dir.path()).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket::default());
            Ok(())
        })
        .unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let barrier = &barrier;
            let store = Store::open(dir.path(), dir.path()).unwrap();
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..50 {
                    store
                        .update(|data| {
                            let old = data.tickets[0].revision_round;
                            std::thread::yield_now();
                            data.tickets[0].revision_round = old + 1;
                            Ok(())
                        })
                        .unwrap();
                }
            });
        }
    });
    assert_eq!(store.read().unwrap().tickets[0].revision_round, 100);
}

#[test]
fn project_hash_stable() {
    use herdr_shepherd::store::project_hash;
    assert_eq!(
        project_hash(std::path::Path::new("/example/project")),
        "b8c7d3b82ccbc8daeeadfe9ec3635316f6a87020a43de2ee83148cc3a9ee6e2e"
    );
    let dir = tempfile::tempdir().unwrap();
    let first = Store::open(dir.path(), dir.path()).unwrap();
    let second = Store::open(dir.path(), &dir.path().join(".")).unwrap();
    assert_eq!(first.root(), second.root());
}

#[test]
fn failed_update_does_not_persist_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), dir.path()).unwrap();
    store
        .update(|data| {
            data.pending_pings.push("committed".into());
            Ok(())
        })
        .unwrap();
    let result: Result<(), _> = store.update(|data| {
        data.pending_pings.push("rolled back".into());
        Err(herdr_shepherd::error::ShepherdError::Usage(
            "invalid report".into(),
        ))
    });
    assert!(result.is_err());
    assert_eq!(store.read().unwrap().pending_pings, ["committed"]);
}

#[test]
fn corrupt_store_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), dir.path()).unwrap();
    store.update(|_| Ok(())).unwrap();
    let path = store.root().join("state.json");
    std::fs::write(&path, "broken").unwrap();
    let error = store.read().unwrap_err().to_string();
    assert!(error.contains(path.to_str().unwrap()));
    assert!(error.contains("restore"));
    assert!(store.update(|_| Ok(())).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "broken");
}

#[test]
fn unsupported_schema_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), dir.path()).unwrap();
    store.update(|_| Ok(())).unwrap();
    let mut data = store.read().unwrap();
    data.schema_version = 3;
    std::fs::write(
        store.root().join("state.json"),
        serde_json::to_vec(&data).unwrap(),
    )
    .unwrap();
    assert!(
        store
            .read()
            .unwrap_err()
            .to_string()
            .contains("schema version 3")
    );
    assert!(store.update(|_| Ok(())).is_err());
}

#[test]
fn invalid_metadata_update_does_not_replace_valid_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), dir.path()).unwrap();
    store
        .update(|data| {
            data.pending_pings.push("valid".into());
            Ok(())
        })
        .unwrap();
    assert!(
        store
            .update(|data| {
                data.schema_version = 3;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(store.read().unwrap().pending_pings, ["valid"]);
}
