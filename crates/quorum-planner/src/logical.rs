//! Logical planning: name resolution, validation, and plan shapes.

use quorum_sql::ast::{BinOp, UnOp};
use quorum_types::{DataType, Schema, Value};

/// A fully-resolved expression: columns are positional indices into the row
/// produced by the operator this expression is attached to.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundExpr {
    Literal(Value),
    Column {
        index: usize,
        name: String,
        data_type: DataType,
    },
    Binary {
        op: BinOp,
        left: Box<BoundExpr>,
        right: Box<BoundExpr>,
    },
    Unary {
        op: UnOp,
        operand: Box<BoundExpr>,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
    },
    InList {
        expr: Box<BoundExpr>,
        list: Vec<BoundExpr>,
        negated: bool,
    },
    Between {
        expr: Box<BoundExpr>,
        low: Box<BoundExpr>,
        high: Box<BoundExpr>,
        negated: bool,
    },
    /// Scalar function (UPPER, LOWER, LENGTH, ABS, ...).
    Func {
        name: String,
        args: Vec<BoundExpr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    pub fn from_name(name: &str) -> Option<AggFunc> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(AggFunc::Count),
            "SUM" => Some(AggFunc::Sum),
            "AVG" => Some(AggFunc::Avg),
            "MIN" => Some(AggFunc::Min),
            "MAX" => Some(AggFunc::Max),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
        }
    }
}

/// An aggregate function bound against its input schema.
/// `arg == None` means `COUNT(*)`.
#[derive(Debug, Clone, PartialEq)]
pub struct AggExpr {
    pub func: AggFunc,
    pub arg: Option<BoundExpr>,
    pub distinct: bool,
}

impl AggExpr {
    /// Stable display label, e.g. `count(*)` or `sum(salary)`.
    pub fn label(&self) -> String {
        match &self.arg {
            None => format!("{}(*)", self.func.name()),
            Some(e) => format!("{}({})", self.func.name(), expr_label(e)),
        }
    }
}

/// Human-readable label for a bound expression (used for output naming).
pub fn expr_label(e: &BoundExpr) -> String {
    match e {
        BoundExpr::Literal(v) => v.to_string(),
        BoundExpr::Column { name, .. } => name.clone(),
        BoundExpr::Binary { op, left, right } => {
            format!("({} {} {})", expr_label(left), op.symbol(), expr_label(right))
        }
        BoundExpr::Unary { op, operand } => match op {
            UnOp::Neg => format!("-{}", expr_label(operand)),
            UnOp::Not => format!("NOT {}", expr_label(operand)),
        },
        BoundExpr::IsNull { expr, negated } => {
            format!("{} IS {}NULL", expr_label(expr), if *negated { "NOT " } else { "" })
        }
        BoundExpr::InList { expr, list, negated } => {
            let items: Vec<String> = list.iter().map(expr_label).collect();
            format!(
                "{} {}IN ({})",
                expr_label(expr),
                if *negated { "NOT " } else { "" },
                items.join(", ")
            )
        }
        BoundExpr::Between { expr, low, high, negated } => format!(
            "{} {}BETWEEN {} AND {}",
            expr_label(expr),
            if *negated { "NOT " } else { "" },
            expr_label(low),
            expr_label(high)
        ),
        BoundExpr::Func { name, args } => {
            let items: Vec<String> = args.iter().map(expr_label).collect();
            format!("{}({})", name.to_lowercase(), items.join(", "))
        }
    }
}

/// Best-effort static type inference (execution is dynamically typed;
/// this only feeds column metadata).
pub fn infer_type(e: &BoundExpr) -> DataType {
    use quorum_sql::ast::BinOp;
    match e {
        BoundExpr::Literal(v) => v.data_type().unwrap_or(DataType::Text),
        BoundExpr::Column { data_type, .. } => *data_type,
        BoundExpr::Binary { op, left, right } => {
            if op.is_logical() || op.is_comparison() {
                DataType::Bool
            } else if matches!(infer_type(left), DataType::Float)
                || matches!(infer_type(right), DataType::Float)
            {
                DataType::Float
            } else {
                DataType::Int
            }
        }
        BoundExpr::Unary { op, operand } => match op {
            quorum_sql::ast::UnOp::Not => DataType::Bool,
            quorum_sql::ast::UnOp::Neg => infer_type(operand),
        },
        BoundExpr::IsNull { .. } | BoundExpr::InList { .. } | BoundExpr::Between { .. } => {
            DataType::Bool
        }
        BoundExpr::Func { name, args } => match name.to_ascii_uppercase().as_str() {
            "LENGTH" => DataType::Int,
            "UPPER" | "LOWER" => DataType::Text,
            "ABS" => args
                .first()
                .map(infer_type)
                .unwrap_or(DataType::Int),
            _ => DataType::Text,
        },
    }
}

#[derive(Debug, Clone)]
pub enum LogicalPlan {
    CreateTable {
        def: quorum_types::TableDef,
        if_not_exists: bool,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    Insert {
        table: String,
        /// Target columns in order (defaults to all table columns).
        columns: Vec<String>,
        source: Box<LogicalPlan>,
    },
    Values {
        rows: Vec<Vec<BoundExpr>>,
        schema: Schema,
    },
    Scan {
        table: String,
        schema: Schema,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: BoundExpr,
    },
    Project {
        input: Box<LogicalPlan>,
        exprs: Vec<(BoundExpr, String)>,
    },
    Update {
        table: String,
        schema: Schema,
        /// (column index, new value expression)
        assignments: Vec<(usize, BoundExpr)>,
        predicate: Option<BoundExpr>,
    },
    Delete {
        table: String,
        schema: Schema,
        predicate: Option<BoundExpr>,
    },
    Sort {
        input: Box<LogicalPlan>,
        /// (sort key, ascending)
        keys: Vec<(BoundExpr, bool)>,
    },
    Limit {
        input: Box<LogicalPlan>,
        limit: Option<u64>,
        offset: u64,
    },
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<BoundExpr>,
        aggregates: Vec<AggExpr>,
        schema: Schema,
    },
}
