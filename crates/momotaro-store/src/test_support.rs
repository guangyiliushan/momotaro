//! Test-only store opening shared by this crate's unit tests.

use crate::Store;

/// Opens a file-backed store without schema; fixture tests stamp their own state.
pub(crate) fn raw_temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let store = Store::open(dir.path().join("momotaro.db")).expect("open store");
    (dir, store)
}

/// Opens an initialized store in its own temporary directory.
pub(crate) fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let mut store = Store::open(dir.path().join("momotaro.db")).expect("open store");
    store.init_schema().expect("init schema");
    (dir, store)
}
