//! The SQL catalog: table definitions and row-id allocation, stored in the
//! same MVCC keyspace as user data (prefix `tbl/` vs `row/`).

use crate::mvcc::{MvccStore, Txn};
use quorum_types::codec::{decode_schema, encode_schema, put_str, put_u64, Reader};
use quorum_types::{DbError, Result, Schema, TableDef};

const TBL_PREFIX: &str = "tbl/";
const ROW_PREFIX: &str = "row/";

#[derive(Debug, Clone)]
pub struct TableEntry {
    pub def: TableDef,
    pub next_rowid: u64,
}

fn norm(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn tbl_key(name: &str) -> Vec<u8> {
    format!("{}{}", TBL_PREFIX, norm(name)).into_bytes()
}

pub fn row_prefix(table: &str) -> Vec<u8> {
    format!("{}{}/", ROW_PREFIX, norm(table)).into_bytes()
}

pub fn row_key(table: &str, rowid: u64) -> Vec<u8> {
    format!("{}{}/{:016x}", ROW_PREFIX, norm(table), rowid).into_bytes()
}

fn encode_entry(entry: &TableEntry) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, &entry.def.name);
    put_u64(&mut out, entry.next_rowid);
    encode_schema(&entry.def.schema, &mut out);
    out
}

fn decode_entry(name: &str, bytes: &[u8]) -> Result<TableEntry> {
    let mut r = Reader::new(bytes);
    let stored_name = r.str()?;
    let next_rowid = r.u64()?;
    let schema = decode_schema(&mut r)?;
    let _ = name;
    Ok(TableEntry {
        def: TableDef {
            name: stored_name,
            schema,
        },
        next_rowid,
    })
}

pub fn get_table(store: &MvccStore, txn: &Txn, name: &str) -> Result<Option<TableEntry>> {
    match store.get(txn, &tbl_key(name))? {
        Some(bytes) => Ok(Some(decode_entry(name, &bytes)?)),
        None => Ok(None),
    }
}

/// Create a table. Returns `true` if created, `false` if it already existed
/// and `if_not_exists` was set; errors if it exists and `if_not_exists` is
/// not set.
pub fn create_table(
    store: &MvccStore,
    txn: &mut Txn,
    def: &TableDef,
    if_not_exists: bool,
) -> Result<bool> {
    if get_table(store, txn, &def.name)?.is_some() {
        if if_not_exists {
            return Ok(false);
        }
        return Err(DbError::Constraint(format!(
            "table {} already exists",
            def.name
        )));
    }
    let entry = TableEntry {
        def: def.clone(),
        next_rowid: 0,
    };
    store.put(txn, &tbl_key(&def.name), encode_entry(&entry));
    Ok(true)
}

/// Drop a table (rows included). Returns `true` if dropped.
pub fn drop_table(store: &MvccStore, txn: &mut Txn, name: &str, if_exists: bool) -> Result<bool> {
    if get_table(store, txn, name)?.is_none() {
        if if_exists {
            return Ok(false);
        }
        return Err(DbError::NotFound(format!("table {} does not exist", name)));
    }
    // Delete rows first (prefix scan of this table's rows).
    let prefix = row_prefix(name);
    let rows = store.scan(txn, &prefix)?;
    for (key, _) in rows {
        store.delete(txn, &key);
    }
    store.delete(txn, &tbl_key(name));
    Ok(true)
}

pub fn list_tables(store: &MvccStore, txn: &Txn) -> Result<Vec<TableEntry>> {
    let prefix = TBL_PREFIX.as_bytes();
    let mut out = Vec::new();
    for (key, value) in store.scan(txn, prefix)? {
        let name = String::from_utf8_lossy(&key[TBL_PREFIX.len()..]).into_owned();
        out.push(decode_entry(&name, &value)?);
    }
    Ok(out)
}

/// Allocate the next row id for `table` within this transaction.
pub fn alloc_rowid(store: &MvccStore, txn: &mut Txn, table: &str) -> Result<u64> {
    let mut entry =
        get_table(store, txn, table)?.ok_or_else(|| DbError::NotFound(format!("table {}", table)))?;
    let id = entry.next_rowid;
    entry.next_rowid += 1;
    store.put(txn, &tbl_key(table), encode_entry(&entry));
    Ok(id)
}

/// Fetch a table's schema (convenience for the binder).
pub fn table_schema(store: &MvccStore, txn: &Txn, name: &str) -> Result<Option<Schema>> {
    Ok(get_table(store, txn, name)?.map(|e| e.def.schema))
}
