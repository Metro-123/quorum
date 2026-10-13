use quorum_storage::catalog;
use quorum_storage::MvccStore;
use quorum_types::{ColumnDef, DataType, DbError, Schema, TableDef};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "quorum-storage-test-{}-{}",
        std::process::id(),
        n
    ))
}

fn cleanup(dir: &PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn put_get_commit() {
    let store = MvccStore::in_memory().unwrap();
    let mut txn = store.begin();
    store.put(&mut txn, b"a", b"1".to_vec());
    store.put(&mut txn, b"b", b"2".to_vec());
    // own writes are visible pre-commit
    assert_eq!(store.get(&txn, b"a").unwrap(), Some(b"1".to_vec()));
    store.commit(txn).unwrap();

    let txn = store.begin();
    assert_eq!(store.get(&txn, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(store.get(&txn, b"missing").unwrap(), None);
}

#[test]
fn snapshot_isolation_read_your_writes_and_history() {
    let store = MvccStore::in_memory().unwrap();

    let mut t1 = store.begin();
    store.put(&mut t1, b"k", b"v1".to_vec());
    store.commit(t1).unwrap();

    // snapshot taken now
    let reader = store.begin();

    let mut t2 = store.begin();
    store.put(&mut t2, b"k", b"v2".to_vec());
    store.commit(t2).unwrap();

    // reader still sees the old version
    assert_eq!(store.get(&reader, b"k").unwrap(), Some(b"v1".to_vec()));
    // fresh snapshot sees the new version
    let fresh = store.begin();
    assert_eq!(store.get(&fresh, b"k").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn write_write_conflict_aborts() {
    let store = MvccStore::in_memory().unwrap();
    let mut t1 = store.begin();
    store.put(&mut t1, b"k", b"v1".to_vec());
    let mut t2 = store.begin();
    store.put(&mut t2, b"k", b"v2".to_vec());

    store.commit(t1).unwrap();
    let err = store.commit(t2).unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "expected conflict, got {:?}", err);
}

#[test]
fn disjoint_writes_do_not_conflict() {
    let store = MvccStore::in_memory().unwrap();
    let mut t1 = store.begin();
    store.put(&mut t1, b"a", b"1".to_vec());
    let mut t2 = store.begin();
    store.put(&mut t2, b"b", b"2".to_vec());
    store.commit(t1).unwrap();
    store.commit(t2).unwrap();
    let txn = store.begin();
    assert_eq!(store.get(&txn, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(store.get(&txn, b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn deletes_and_tombstones() {
    let store = MvccStore::in_memory().unwrap();
    let mut t1 = store.begin();
    store.put(&mut t1, b"k", b"v".to_vec());
    store.commit(t1).unwrap();

    let mut t2 = store.begin();
    store.delete(&mut t2, b"k");
    assert_eq!(store.get(&t2, b"k").unwrap(), None); // own delete visible
    store.commit(t2).unwrap();

    let t3 = store.begin();
    assert_eq!(store.get(&t3, b"k").unwrap(), None);
    let scanned = store.scan(&t3, b"").unwrap();
    assert!(scanned.is_empty());
}

#[test]
fn prefix_scan_respects_snapshot_and_writes() {
    let store = MvccStore::in_memory().unwrap();
    let mut t1 = store.begin();
    store.put(&mut t1, b"row/t/00", b"a".to_vec());
    store.put(&mut t1, b"row/t/01", b"b".to_vec());
    store.put(&mut t1, b"other/x", b"c".to_vec());
    store.commit(t1).unwrap();

    let mut t2 = store.begin();
    store.put(&mut t2, b"row/t/02", b"c".to_vec());
    let rows = store.scan(&t2, b"row/t/").unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].1, b"a");
    assert_eq!(rows[2].1, b"c");
}

#[test]
fn wal_durability_across_reopen() {
    let dir = temp_dir();
    cleanup(&dir);

    {
        let store = MvccStore::open(&dir).unwrap();
        let mut txn = store.begin();
        store.put(&mut txn, b"durable", b"yes".to_vec());
        store.commit(txn).unwrap();
        let mut txn = store.begin();
        store.put(&mut txn, b"durable2", b"also".to_vec());
        store.delete(&mut txn, b"gone");
        store.commit(txn).unwrap();
    } // store dropped -> file closed

    let store = MvccStore::open(&dir).unwrap();
    let txn = store.begin();
    assert_eq!(store.get(&txn, b"durable").unwrap(), Some(b"yes".to_vec()));
    assert_eq!(store.get(&txn, b"durable2").unwrap(), Some(b"also".to_vec()));
    cleanup(&dir);
}

#[test]
fn torn_wal_tail_is_tolerated() {
    let dir = temp_dir();
    cleanup(&dir);
    {
        let store = MvccStore::open(&dir).unwrap();
        let mut txn = store.begin();
        store.put(&mut txn, b"good", b"1".to_vec());
        store.commit(txn).unwrap();
    }
    // simulate a crash mid-write: append garbage bytes
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("wal.log"))
        .unwrap();
    f.write_all(&[0xAA, 0xBB, 0xCC, 0xDD, 0x01]).unwrap();
    f.sync_all().unwrap();
    drop(f);

    let store = MvccStore::open(&dir).unwrap();
    let txn = store.begin();
    assert_eq!(store.get(&txn, b"good").unwrap(), Some(b"1".to_vec()));
    // and the store still accepts new writes
    let mut txn = store.begin();
    store.put(&mut txn, b"after-crash", b"ok".to_vec());
    store.commit(txn).unwrap();
    cleanup(&dir);
}

#[test]
fn catalog_lifecycle() {
    let store = MvccStore::in_memory().unwrap();
    let def = TableDef {
        name: "Users".into(),
        schema: Schema::new(vec![
            ColumnDef::new("id", DataType::Int),
            ColumnDef::new("name", DataType::Text),
        ]),
    };

    let mut txn = store.begin();
    assert!(catalog::create_table(&store, &mut txn, &def, false).unwrap());
    store.commit(txn).unwrap();

    // duplicate create errors
    let mut txn = store.begin();
    let err = catalog::create_table(&store, &mut txn, &def, false).unwrap_err();
    assert!(matches!(err, DbError::Constraint(_)));
    store.abort(txn);

    // if_not_exists is a no-op
    let mut txn = store.begin();
    assert!(!catalog::create_table(&store, &mut txn, &def, true).unwrap());
    store.abort(txn);

    // rowid allocation persists across txns
    let mut txn = store.begin();
    assert_eq!(catalog::alloc_rowid(&store, &mut txn, "users").unwrap(), 0);
    assert_eq!(catalog::alloc_rowid(&store, &mut txn, "users").unwrap(), 1);
    store.commit(txn).unwrap();
    let mut txn = store.begin();
    assert_eq!(catalog::alloc_rowid(&store, &mut txn, "users").unwrap_or(2), 2);

    // listing is case-insensitive
    let txn = store.begin();
    let tables = catalog::list_tables(&store, &txn).unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].def.name, "Users");

    // drop removes table and rows
    let mut txn = store.begin();
    let key = catalog::row_key("users", 0);
    store.put(&mut txn, &key, vec![1, 2, 3]);
    assert!(catalog::drop_table(&store, &mut txn, "USERS", false).unwrap());
    store.commit(txn).unwrap();
    let txn = store.begin();
    assert!(catalog::get_table(&store, &txn, "users").unwrap().is_none());
    assert_eq!(store.get(&txn, &key).unwrap(), None);
}
