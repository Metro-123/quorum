//! The SQL database facade: parse -> bind -> optimize -> execute -> commit.

use crate::operators::{build, ExecCtx, Row};
use quorum_planner::{bind, is_write, optimize, output_schema, SchemaProvider};
use quorum_sql::ast::Statement;
use quorum_storage::{catalog, MvccStore};
use quorum_types::{Result, Schema};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    /// Command tag for writes, e.g. `INSERT 3`; or `SELECT n` for reads.
    pub message: String,
}

/// A single-node SQL database. Hook for distribution: every statement runs
/// in its own MVCC transaction; writes go through the WAL before commit.
pub struct Database {
    store: Arc<MvccStore>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Database> {
        Ok(Database {
            store: Arc::new(MvccStore::open(path)?),
        })
    }

    pub fn in_memory() -> Result<Database> {
        Ok(Database {
            store: Arc::new(MvccStore::in_memory()?),
        })
    }

    pub fn store(&self) -> Arc<MvccStore> {
        Arc::clone(&self.store)
    }

    /// Execute a script; each statement is its own transaction.
    pub fn execute(&self, sql: &str) -> Result<Vec<ResultSet>> {
        let statements = quorum_sql::parse(sql)?;
        let mut out = Vec::with_capacity(statements.len());
        for stmt in &statements {
            out.push(self.execute_stmt(stmt)?);
        }
        Ok(out)
    }

    pub fn execute_stmt(&self, stmt: &Statement) -> Result<ResultSet> {
        let logical = bind(stmt, self)?;
        let physical = optimize(logical);
        let write = is_write(&physical);
        let columns = output_schema(&physical).names();

        let mut txn = self.store.begin();

        let run = (|| -> Result<Vec<Row>> {
            let mut op = build(physical, &self.store)?;
            let mut ctx = ExecCtx {
                store: &self.store,
                txn: &mut txn,
            };
            let mut rows = Vec::new();
            while let Some(row) = op.next(&mut ctx)? {
                rows.push(row);
            }
            Ok(rows)
        })();

        match run {
            Ok(rows) => {
                self.store.commit(txn)?;
                if write {
                    let message = rows
                        .first()
                        .and_then(|r| r.first())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "OK".to_string());
                    Ok(ResultSet {
                        columns: Vec::new(),
                        rows: Vec::new(),
                        message,
                    })
                } else {
                    let message = format!("SELECT {}", rows.len());
                    Ok(ResultSet {
                        columns,
                        rows,
                        message,
                    })
                }
            }
            Err(e) => {
                self.store.abort(txn);
                Err(e)
            }
        }
    }
}

impl SchemaProvider for Database {
    fn table_schema(&self, name: &str) -> Result<Option<Schema>> {
        let txn = self.store.begin();
        catalog::table_schema(&self.store, &txn, name)
    }
}
