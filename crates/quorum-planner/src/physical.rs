//! Physical planning.
//!
//! Today this is a faithful 1:1 lowering of the logical plan: it exists as the
//! seam where access-path selection, predicate pushdown, and cost-based
//! choices will live. `Distribution` records where a plan runs; it is the
//! hook for pushing fragments to storage nodes.

use crate::logical::{infer_type, AggExpr, BoundExpr, LogicalPlan};
use quorum_types::{ColumnDef, Schema, TableDef};

/// Where an operator executes. Single-node for now; the distributed
/// executor will fan plans out across both variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distribution {
    /// Runs on the coordinator / owning node.
    Single,
    /// Placeholder: reserved for remote fragments (phase 4).
    Remote(u32),
}

#[derive(Debug, Clone)]
pub enum PhysicalPlan {
    CreateTable {
        def: TableDef,
        if_not_exists: bool,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    Insert {
        table: String,
        columns: Vec<String>,
        source: Box<PhysicalPlan>,
    },
    Values {
        rows: Vec<Vec<BoundExpr>>,
        schema: Schema,
    },
    Scan {
        table: String,
        schema: Schema,
        distribution: Distribution,
    },
    Filter {
        input: Box<PhysicalPlan>,
        predicate: BoundExpr,
    },
    Project {
        input: Box<PhysicalPlan>,
        exprs: Vec<(BoundExpr, String)>,
    },
    Update {
        table: String,
        schema: Schema,
        assignments: Vec<(usize, BoundExpr)>,
        predicate: Option<BoundExpr>,
    },
    Delete {
        table: String,
        schema: Schema,
        predicate: Option<BoundExpr>,
    },
    Sort {
        input: Box<PhysicalPlan>,
        keys: Vec<(BoundExpr, bool)>,
    },
    Limit {
        input: Box<PhysicalPlan>,
        limit: Option<u64>,
        offset: u64,
    },
    Aggregate {
        input: Box<PhysicalPlan>,
        group_by: Vec<BoundExpr>,
        aggregates: Vec<AggExpr>,
        schema: Schema,
    },
}

/// Lower a logical plan to a physical one.
pub fn optimize(logical: LogicalPlan) -> PhysicalPlan {
    match logical {
        LogicalPlan::CreateTable { def, if_not_exists } => {
            PhysicalPlan::CreateTable { def, if_not_exists }
        }
        LogicalPlan::DropTable { name, if_exists } => {
            PhysicalPlan::DropTable { name, if_exists }
        }
        LogicalPlan::Insert {
            table,
            columns,
            source,
        } => PhysicalPlan::Insert {
            table,
            columns,
            source: Box::new(optimize(*source)),
        },
        LogicalPlan::Values { rows, schema } => PhysicalPlan::Values { rows, schema },
        LogicalPlan::Scan { table, schema } => PhysicalPlan::Scan {
            table,
            schema,
            distribution: Distribution::Single,
        },
        LogicalPlan::Filter { input, predicate } => PhysicalPlan::Filter {
            input: Box::new(optimize(*input)),
            predicate,
        },
        LogicalPlan::Project { input, exprs } => PhysicalPlan::Project {
            input: Box::new(optimize(*input)),
            exprs,
        },
        LogicalPlan::Update {
            table,
            schema,
            assignments,
            predicate,
        } => PhysicalPlan::Update {
            table,
            schema,
            assignments,
            predicate,
        },
        LogicalPlan::Delete {
            table,
            schema,
            predicate,
        } => PhysicalPlan::Delete {
            table,
            schema,
            predicate,
        },
        LogicalPlan::Sort { input, keys } => PhysicalPlan::Sort {
            input: Box::new(optimize(*input)),
            keys,
        },
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => PhysicalPlan::Limit {
            input: Box::new(optimize(*input)),
            limit,
            offset,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            schema,
        } => PhysicalPlan::Aggregate {
            input: Box::new(optimize(*input)),
            group_by,
            aggregates,
            schema,
        },
    }
}

/// The schema of rows produced by this plan.
pub fn output_schema(plan: &PhysicalPlan) -> Schema {
    match plan {
        PhysicalPlan::Values { schema, .. } => schema.clone(),
        PhysicalPlan::Scan { schema, .. } => schema.clone(),
        PhysicalPlan::Filter { input, .. } => output_schema(input),
        PhysicalPlan::Sort { input, .. } => output_schema(input),
        PhysicalPlan::Limit { input, .. } => output_schema(input),
        PhysicalPlan::Aggregate { schema, .. } => schema.clone(),
        PhysicalPlan::Project { exprs, .. } => Schema::new(
            exprs
                .iter()
                .map(|(e, name)| ColumnDef {
                    name: name.clone(),
                    data_type: infer_type(e),
                    nullable: true,
                    primary_key: false,
                })
                .collect(),
        ),
        PhysicalPlan::CreateTable { .. }
        | PhysicalPlan::DropTable { .. }
        | PhysicalPlan::Insert { .. }
        | PhysicalPlan::Update { .. }
        | PhysicalPlan::Delete { .. } => Schema::new(Vec::new()),
    }
}

/// True if this plan mutates data or schema.
pub fn is_write(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::Values { .. }
        | PhysicalPlan::Scan { .. }
        | PhysicalPlan::Filter { .. }
        | PhysicalPlan::Project { .. }
        | PhysicalPlan::Sort { .. }
        | PhysicalPlan::Limit { .. }
        | PhysicalPlan::Aggregate { .. } => false,
        _ => true,
    }
}
