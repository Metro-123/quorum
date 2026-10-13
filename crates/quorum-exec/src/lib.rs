//! Query execution: the volcano operator model plus the SQL database facade.

pub mod database;
pub mod eval;
pub mod operators;

pub use database::{Database, ResultSet};
pub use operators::{build, ExecCtx, Operator, Row};
