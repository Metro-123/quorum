//! Query planning: binding (name resolution) and logical -> physical lowering.

pub mod binder;
pub mod logical;
pub mod physical;

pub use binder::{bind, SchemaProvider};
pub use logical::{AggExpr, AggFunc, BoundExpr, LogicalPlan};
pub use physical::{is_write, optimize, output_schema, Distribution, PhysicalPlan};
