//! Volcano-style (pull) execution operators.

use crate::eval::eval;
use quorum_planner::logical::infer_type;
use quorum_planner::{AggExpr, AggFunc, BoundExpr, PhysicalPlan};
use quorum_storage::{catalog, MvccStore, Txn};
use quorum_types::codec::{decode_row, encode_row};
use quorum_types::codec::encode_value;
use quorum_types::{ColumnDef, DbError, Result, Schema, Value};
use std::cmp::Ordering;
use std::collections::BTreeMap;

pub type Row = Vec<Value>;

/// Execution context: shared storage handle plus the running transaction.
pub struct ExecCtx<'a> {
    pub store: &'a MvccStore,
    pub txn: &'a mut Txn,
}

pub trait Operator {
    /// Schema of the rows this operator emits.
    fn schema(&self) -> &Schema;
    /// Pull the next row. `None` = exhausted. May mutate via `ctx.txn`.
    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>>;
}

fn empty_schema() -> Schema {
    Schema::new(Vec::new())
}

fn tag(text: String) -> Option<Row> {
    Some(vec![Value::Text(text)])
}

// ---------------------------------------------------------------------------
// leaves
// ---------------------------------------------------------------------------

/// Emits constant rows (INSERT ... VALUES and FROM-less SELECT).
pub struct ValuesOp {
    rows: Vec<Vec<BoundExpr>>,
    schema: Schema,
    idx: usize,
}

impl Operator for ValuesOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self, _ctx: &mut ExecCtx) -> Result<Option<Row>> {
        if self.idx >= self.rows.len() {
            return Ok(None);
        }
        let exprs = &self.rows[self.idx];
        self.idx += 1;
        let mut row = Vec::with_capacity(exprs.len());
        for e in exprs {
            row.push(eval(e, &[])?);
        }
        Ok(Some(row))
    }
}

/// Full scan of a table's rows, in row-id (insertion) order. The row set is
/// materialized on first pull and read through the transaction snapshot.
pub struct TableScanOp {
    table: String,
    schema: Schema,
    rows: Option<std::vec::IntoIter<Row>>,
}

impl Operator for TableScanOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        if self.rows.is_none() {
            let prefix = catalog::row_prefix(&self.table);
            let entries = ctx.store.scan(ctx.txn, &prefix)?;
            let mut rows = Vec::with_capacity(entries.len());
            for (_key, value) in entries {
                let mut r = quorum_types::codec::Reader::new(&value);
                rows.push(decode_row(&mut r)?);
            }
            self.rows = Some(rows.into_iter());
        }
        Ok(self.rows.as_mut().unwrap().next())
    }
}

// ---------------------------------------------------------------------------
// transforms
// ---------------------------------------------------------------------------

pub struct FilterOp {
    input: Box<dyn Operator>,
    schema: Schema,
    predicate: BoundExpr,
}

impl Operator for FilterOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        loop {
            match self.input.next(ctx)? {
                None => return Ok(None),
                Some(row) => {
                    if eval(&self.predicate, &row)?.truthy()? {
                        return Ok(Some(row));
                    }
                }
            }
        }
    }
}

pub struct ProjectOp {
    input: Box<dyn Operator>,
    schema: Schema,
    exprs: Vec<(BoundExpr, String)>,
}

impl Operator for ProjectOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        match self.input.next(ctx)? {
            None => Ok(None),
            Some(row) => {
                let mut out = Vec::with_capacity(self.exprs.len());
                for (e, _) in &self.exprs {
                    out.push(eval(e, &row)?);
                }
                Ok(Some(out))
            }
        }
    }
}

pub struct LimitOp {
    input: Box<dyn Operator>,
    schema: Schema,
    limit: Option<u64>,
    offset: u64,
    skipped: u64,
    emitted: u64,
}

impl Operator for LimitOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        while self.skipped < self.offset {
            match self.input.next(ctx)? {
                None => return Ok(None),
                Some(_) => self.skipped += 1,
            }
        }
        if let Some(l) = self.limit {
            if self.emitted >= l {
                return Ok(None);
            }
        }
        match self.input.next(ctx)? {
            None => Ok(None),
            Some(row) => {
                self.emitted += 1;
                Ok(Some(row))
            }
        }
    }
}

pub struct SortOp {
    input: Box<dyn Operator>,
    schema: Schema,
    keys: Vec<(BoundExpr, bool)>,
    buffer: Option<std::vec::IntoIter<Row>>,
}

impl Operator for SortOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        if self.buffer.is_none() {
            let mut rows = Vec::new();
            while let Some(row) = self.input.next(ctx)? {
                let mut key_vals = Vec::with_capacity(self.keys.len());
                for (e, _) in &self.keys {
                    key_vals.push(eval(e, &row)?);
                }
                rows.push((key_vals, row));
            }
            let dirs: Vec<bool> = self.keys.iter().map(|(_, asc)| *asc).collect();
            rows.sort_by(|a, b| cmp_keys(&a.0, &b.0, &dirs));
            self.buffer = Some(rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>().into_iter());
        }
        Ok(self.buffer.as_mut().unwrap().next())
    }
}

fn cmp_keys(a: &[Value], b: &[Value], dirs: &[bool]) -> Ordering {
    for (i, (av, bv)) in a.iter().zip(b.iter()).enumerate() {
        let base = match av.compare(bv) {
            Some(o) => o,
            None => match (av.is_null(), bv.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                // incomparable non-nulls: keep stable
                (false, false) => Ordering::Equal,
            },
        };
        let o = if *dirs.get(i).unwrap_or(&true) {
            base
        } else {
            base.reverse()
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

// ---------------------------------------------------------------------------
// aggregation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AggAccum {
    count: i64,
    sum_i: i128,
    sum_f: f64,
    any_float: bool,
    min: Option<Value>,
    max: Option<Value>,
}

impl AggAccum {
    fn update(&mut self, agg: &AggExpr, row: &[Value]) -> Result<()> {
        let v = match &agg.arg {
            None => Value::Int(1), // COUNT(*)
            Some(e) => eval(e, row)?,
        };
        match agg.func {
            AggFunc::Count => {
                if !v.is_null() {
                    self.count += 1;
                }
            }
            AggFunc::Sum | AggFunc::Avg => {
                if v.is_null() {
                    return Ok(());
                }
                match v {
                    Value::Int(i) => self.sum_i += i as i128,
                    Value::Float(f) => {
                        self.sum_f += f;
                        self.any_float = true;
                    }
                    other => {
                        return Err(DbError::Type(format!(
                            "{} requires a numeric argument, got {}",
                            agg.func.name(),
                            other.type_name()
                        )))
                    }
                }
                self.count += 1;
            }
            AggFunc::Min => {
                if !v.is_null() {
                    let replace = match &self.min {
                        None => true,
                        Some(m) => matches!(v.compare(m), Some(Ordering::Less)),
                    };
                    if replace {
                        self.min = Some(v);
                    }
                }
            }
            AggFunc::Max => {
                if !v.is_null() {
                    let replace = match &self.max {
                        None => true,
                        Some(m) => matches!(v.compare(m), Some(Ordering::Greater)),
                    };
                    if replace {
                        self.max = Some(v);
                    }
                }
            }
        }
        Ok(())
    }

    fn finalize(&self, func: AggFunc) -> Result<Value> {
        match func {
            AggFunc::Count => Ok(Value::Int(self.count)),
            AggFunc::Sum => {
                if self.count == 0 {
                    Ok(Value::Null)
                } else if self.any_float {
                    Ok(Value::Float(self.sum_i as f64 + self.sum_f))
                } else {
                    i64::try_from(self.sum_i)
                        .map(Value::Int)
                        .map_err(|_| DbError::Type("SUM overflow".into()))
                }
            }
            AggFunc::Avg => {
                if self.count == 0 {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Float(
                        (self.sum_i as f64 + self.sum_f) / self.count as f64,
                    ))
                }
            }
            AggFunc::Min => Ok(self.min.clone().unwrap_or(Value::Null)),
            AggFunc::Max => Ok(self.max.clone().unwrap_or(Value::Null)),
        }
    }
}

pub struct AggregateOp {
    input: Box<dyn Operator>,
    schema: Schema,
    group_by: Vec<BoundExpr>,
    aggregates: Vec<AggExpr>,
    buffer: Option<std::vec::IntoIter<Row>>,
}

impl Operator for AggregateOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        if self.buffer.is_none() {
            let mut order: Vec<(Vec<Value>, Vec<AggAccum>)> = Vec::new();
            let mut index: BTreeMap<Vec<u8>, usize> = BTreeMap::new();

            loop {
                let row = match self.input.next(ctx)? {
                    Some(r) => r,
                    None => break,
                };
                let mut key_vals = Vec::with_capacity(self.group_by.len());
                for g in &self.group_by {
                    key_vals.push(eval(g, &row)?);
                }
                let mut key = Vec::new();
                for v in &key_vals {
                    encode_value(v, &mut key);
                }
                let idx = match index.get(&key) {
                    Some(i) => *i,
                    None => {
                        let i = order.len();
                        let accs = self.aggregates.iter().map(|_| AggAccum::default()).collect();
                        index.insert(key, i);
                        order.push((key_vals, accs));
                        i
                    }
                };
                for (agg, acc) in self.aggregates.iter().zip(order[idx].1.iter_mut()) {
                    acc.update(agg, &row)?;
                }
            }

            // Global aggregate over zero rows still produces one row.
            if order.is_empty() && self.group_by.is_empty() {
                let accs = self.aggregates.iter().map(|_| AggAccum::default()).collect();
                order.push((Vec::new(), accs));
            }

            let mut out = Vec::with_capacity(order.len());
            for (key_vals, accs) in order {
                let mut row = key_vals;
                for (agg, acc) in self.aggregates.iter().zip(accs.iter()) {
                    row.push(acc.finalize(agg.func)?);
                }
                out.push(row);
            }
            self.buffer = Some(out.into_iter());
        }
        Ok(self.buffer.as_mut().unwrap().next())
    }
}

// ---------------------------------------------------------------------------
// writes
// ---------------------------------------------------------------------------

pub struct InsertOp {
    table: String,
    target_indices: Vec<usize>,
    table_schema: Schema,
    source: Box<dyn Operator>,
    out_schema: Schema,
    done: bool,
}

impl Operator for InsertOp {
    fn schema(&self) -> &Schema {
        &self.out_schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;

        let mut source_rows = Vec::new();
        while let Some(r) = self.source.next(ctx)? {
            source_rows.push(r);
        }

        let mut count = 0u64;
        for row in source_rows {
            let mut full = vec![Value::Null; self.table_schema.columns.len()];
            for (i, value) in row.iter().enumerate() {
                let col = &self.table_schema.columns[self.target_indices[i]];
                if value.is_null() && !col.nullable {
                    return Err(DbError::Constraint(format!(
                        "column {} cannot be null",
                        col.name
                    )));
                }
                full[self.target_indices[i]] = value.coerce(col.data_type)?;
            }
            for (idx, col) in self.table_schema.columns.iter().enumerate() {
                if full[idx].is_null() {
                    if col.primary_key {
                        return Err(DbError::Constraint(format!(
                            "primary key {} cannot be null",
                            col.name
                        )));
                    }
                    if !col.nullable {
                        return Err(DbError::Constraint(format!(
                            "column {} cannot be null",
                            col.name
                        )));
                    }
                }
            }
            let rowid = catalog::alloc_rowid(ctx.store, ctx.txn, &self.table)?;
            let key = catalog::row_key(&self.table, rowid);
            let mut buf = Vec::new();
            encode_row(&full, &mut buf);
            ctx.store.put(ctx.txn, &key, buf);
            count += 1;
        }
        Ok(tag(format!("INSERT {}", count)))
    }
}

pub struct UpdateOp {
    table: String,
    table_schema: Schema,
    assignments: Vec<(usize, BoundExpr)>,
    predicate: Option<BoundExpr>,
    out_schema: Schema,
    done: bool,
}

impl Operator for UpdateOp {
    fn schema(&self) -> &Schema {
        &self.out_schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;

        let prefix = catalog::row_prefix(&self.table);
        let entries = ctx.store.scan(ctx.txn, &prefix)?;
        let mut count = 0u64;
        for (key, value) in entries {
            let mut r = quorum_types::codec::Reader::new(&value);
            let row = decode_row(&mut r)?;
            let matches = match &self.predicate {
                Some(p) => eval(p, &row)?.truthy()?,
                None => true,
            };
            if !matches {
                continue;
            }
            // assignments are evaluated against the old row, then applied
            let mut new_row = row.clone();
            for (idx, expr) in &self.assignments {
                let v = eval(expr, &row)?;
                let col = &self.table_schema.columns[*idx];
                if v.is_null() && !col.nullable {
                    return Err(DbError::Constraint(format!(
                        "column {} cannot be null",
                        col.name
                    )));
                }
                new_row[*idx] = v.coerce(col.data_type)?;
            }
            let mut buf = Vec::new();
            encode_row(&new_row, &mut buf);
            ctx.store.put(ctx.txn, &key, buf);
            count += 1;
        }
        Ok(tag(format!("UPDATE {}", count)))
    }
}

pub struct DeleteOp {
    table: String,
    predicate: Option<BoundExpr>,
    out_schema: Schema,
    done: bool,
}

impl Operator for DeleteOp {
    fn schema(&self) -> &Schema {
        &self.out_schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;

        let prefix = catalog::row_prefix(&self.table);
        let entries = ctx.store.scan(ctx.txn, &prefix)?;
        let mut count = 0u64;
        for (key, value) in entries {
            let matches = match &self.predicate {
                Some(p) => {
                    let mut r = quorum_types::codec::Reader::new(&value);
                    let row = decode_row(&mut r)?;
                    eval(p, &row)?.truthy()?
                }
                None => true,
            };
            if matches {
                ctx.store.delete(ctx.txn, &key);
                count += 1;
            }
        }
        Ok(tag(format!("DELETE {}", count)))
    }
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

pub struct CreateTableOp {
    def: quorum_types::TableDef,
    if_not_exists: bool,
    out_schema: Schema,
    done: bool,
}

impl Operator for CreateTableOp {
    fn schema(&self) -> &Schema {
        &self.out_schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        let created = catalog::create_table(ctx.store, ctx.txn, &self.def, self.if_not_exists)?;
        let msg = if created {
            "CREATE TABLE"
        } else {
            "CREATE TABLE (already exists)"
        };
        Ok(tag(msg.to_string()))
    }
}

pub struct DropTableOp {
    name: String,
    if_exists: bool,
    out_schema: Schema,
    done: bool,
}

impl Operator for DropTableOp {
    fn schema(&self) -> &Schema {
        &self.out_schema
    }

    fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        let dropped = catalog::drop_table(ctx.store, ctx.txn, &self.name, self.if_exists)?;
        let msg = if dropped {
            "DROP TABLE"
        } else {
            "DROP TABLE (did not exist)"
        };
        Ok(tag(msg.to_string()))
    }
}

// ---------------------------------------------------------------------------
// plan -> operator tree
// ---------------------------------------------------------------------------

pub fn build(plan: PhysicalPlan, store: &MvccStore) -> Result<Box<dyn Operator>> {
    Ok(match plan {
        PhysicalPlan::CreateTable { def, if_not_exists } => Box::new(CreateTableOp {
            def,
            if_not_exists,
            out_schema: empty_schema(),
            done: false,
        }),
        PhysicalPlan::DropTable { name, if_exists } => Box::new(DropTableOp {
            name,
            if_exists,
            out_schema: empty_schema(),
            done: false,
        }),
        PhysicalPlan::Values { rows, schema } => Box::new(ValuesOp {
            rows,
            schema,
            idx: 0,
        }),
        PhysicalPlan::Scan { table, schema, .. } => Box::new(TableScanOp {
            table,
            schema,
            rows: None,
        }),
        PhysicalPlan::Filter { input, predicate } => {
            let child = build(*input, store)?;
            let schema = child.schema().clone();
            Box::new(FilterOp {
                input: child,
                schema,
                predicate,
            })
        }
        PhysicalPlan::Project { input, exprs } => {
            let child = build(*input, store)?;
            let schema = Schema::new(
                exprs
                    .iter()
                    .map(|(e, name)| ColumnDef {
                        name: name.clone(),
                        data_type: infer_type(e),
                        nullable: true,
                        primary_key: false,
                    })
                    .collect(),
            );
            Box::new(ProjectOp {
                input: child,
                schema,
                exprs,
            })
        }
        PhysicalPlan::Sort { input, keys } => {
            let child = build(*input, store)?;
            let schema = child.schema().clone();
            Box::new(SortOp {
                input: child,
                schema,
                keys,
                buffer: None,
            })
        }
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            let child = build(*input, store)?;
            let schema = child.schema().clone();
            Box::new(LimitOp {
                input: child,
                schema,
                limit,
                offset,
                skipped: 0,
                emitted: 0,
            })
        }
        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            schema,
        } => {
            let child = build(*input, store)?;
            Box::new(AggregateOp {
                input: child,
                schema,
                group_by,
                aggregates,
                buffer: None,
            })
        }
        PhysicalPlan::Insert {
            table,
            columns,
            source,
        } => {
            let child = build(*source, store)?;
            let probe = store.begin();
            let table_schema = catalog::table_schema(store, &probe, &table)?
                .ok_or_else(|| DbError::NotFound(format!("table {} does not exist", table)))?;
            let mut target_indices = Vec::with_capacity(columns.len());
            for c in &columns {
                let i = table_schema
                    .column_index(c)
                    .ok_or_else(|| DbError::Bind(format!("unknown column {}", c)))?;
                target_indices.push(i);
            }
            Box::new(InsertOp {
                table,
                target_indices,
                table_schema,
                source: child,
                out_schema: empty_schema(),
                done: false,
            })
        }
        PhysicalPlan::Update {
            table,
            schema,
            assignments,
            predicate,
        } => Box::new(UpdateOp {
            table,
            table_schema: schema,
            assignments,
            predicate,
            out_schema: empty_schema(),
            done: false,
        }),
        PhysicalPlan::Delete {
            table,
            schema: _,
            predicate,
        } => Box::new(DeleteOp {
            table,
            predicate,
            out_schema: empty_schema(),
            done: false,
        }),
    })
}
