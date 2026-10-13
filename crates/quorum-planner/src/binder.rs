//! Binding: AST -> LogicalPlan with resolved column positions and
//! validated semantics.

use crate::logical::{infer_type, AggExpr, AggFunc, BoundExpr, LogicalPlan};
use quorum_sql::ast::{self, Expr, InsertStmt, SelectItem, SelectStmt, Statement};
use quorum_types::{ColumnDef, DataType, DbError, Result, Schema, TableDef, Value};

/// Provides table schemas to the binder.
pub trait SchemaProvider {
    fn table_schema(&self, name: &str) -> Result<Option<Schema>>;
}

pub fn bind(stmt: &Statement, provider: &dyn SchemaProvider) -> Result<LogicalPlan> {
    match stmt {
        Statement::CreateTable(ct) => bind_create_table(ct),
        Statement::DropTable(dt) => Ok(LogicalPlan::DropTable {
            name: dt.name.clone(),
            if_exists: dt.if_exists,
        }),
        Statement::Insert(i) => bind_insert(i, provider),
        Statement::Select(s) => bind_select(s, provider),
        Statement::Update(u) => bind_update(u, provider),
        Statement::Delete(d) => bind_delete(d, provider),
    }
}

fn empty_schema() -> Schema {
    Schema::new(Vec::new())
}

// ---------------------------------------------------------------------------
// plain expression binding
// ---------------------------------------------------------------------------

fn bind_expr(expr: &Expr, schema: &Schema, table: Option<&str>) -> Result<BoundExpr> {
    match expr {
        Expr::Literal(v) => Ok(BoundExpr::Literal(v.clone())),
        Expr::Column { table: qual, name } => {
            if let Some(q) = qual {
                match table {
                    Some(t) if t.eq_ignore_ascii_case(q) => {}
                    Some(_) => {
                        return Err(DbError::Bind(format!(
                            "unknown table prefix {}",
                            q
                        )))
                    }
                    None => {
                        return Err(DbError::Bind(format!(
                            "qualified column {} requires a FROM clause",
                            name
                        )))
                    }
                }
            }
            let idx = schema
                .column_index(name)
                .ok_or_else(|| DbError::Bind(format!("unknown column {}", name)))?;
            let col = &schema.columns[idx];
            Ok(BoundExpr::Column {
                index: idx,
                name: col.name.clone(),
                data_type: col.data_type,
            })
        }
        Expr::Binary { op, left, right } => Ok(BoundExpr::Binary {
            op: *op,
            left: Box::new(bind_expr(left, schema, table)?),
            right: Box::new(bind_expr(right, schema, table)?),
        }),
        Expr::Unary { op, operand } => Ok(BoundExpr::Unary {
            op: *op,
            operand: Box::new(bind_expr(operand, schema, table)?),
        }),
        Expr::IsNull { expr, negated } => Ok(BoundExpr::IsNull {
            expr: Box::new(bind_expr(expr, schema, table)?),
            negated: *negated,
        }),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let mut bound = Vec::with_capacity(list.len());
            for e in list {
                bound.push(bind_expr(e, schema, table)?);
            }
            Ok(BoundExpr::InList {
                expr: Box::new(bind_expr(expr, schema, table)?),
                list: bound,
                negated: *negated,
            })
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Ok(BoundExpr::Between {
            expr: Box::new(bind_expr(expr, schema, table)?),
            low: Box::new(bind_expr(low, schema, table)?),
            high: Box::new(bind_expr(high, schema, table)?),
            negated: *negated,
        }),
        Expr::Call {
            name,
            args,
            distinct,
        } => {
            if ast::is_aggregate(name) {
                return Err(DbError::Bind(format!(
                    "aggregate function {} is not allowed in this context",
                    name
                )));
            }
            bind_scalar_call(name, args, *distinct, schema, table)
        }
    }
}

fn scalar_arity(name: &str) -> Option<usize> {
    match name.to_ascii_uppercase().as_str() {
        "UPPER" | "LOWER" | "LENGTH" | "ABS" => Some(1),
        _ => None,
    }
}

fn bind_scalar_call(
    name: &str,
    args: &[Expr],
    distinct: bool,
    schema: &Schema,
    table: Option<&str>,
) -> Result<BoundExpr> {
    if distinct {
        return Err(DbError::Bind(
            "DISTINCT is only supported inside aggregates".into(),
        ));
    }
    let arity = scalar_arity(name)
        .ok_or_else(|| DbError::Bind(format!("unknown function {}", name)))?;
    if args.len() != arity {
        return Err(DbError::Bind(format!(
            "{} expects {} argument(s), got {}",
            name,
            arity,
            args.len()
        )));
    }
    let mut bound = Vec::with_capacity(args.len());
    for a in args {
        bound.push(bind_expr(a, schema, table)?);
    }
    Ok(BoundExpr::Func {
        name: name.to_ascii_uppercase(),
        args: bound,
    })
}

// ---------------------------------------------------------------------------
// statements
// ---------------------------------------------------------------------------

fn bind_create_table(ct: &ast::CreateTableStmt) -> Result<LogicalPlan> {
    let mut columns = Vec::with_capacity(ct.columns.len());
    for (i, c) in ct.columns.iter().enumerate() {
        if ct.columns[..i]
            .iter()
            .any(|x| x.name.eq_ignore_ascii_case(&c.name))
        {
            return Err(DbError::Bind(format!("duplicate column {}", c.name)));
        }
        columns.push(ColumnDef {
            name: c.name.clone(),
            data_type: c.data_type,
            nullable: c.nullable,
            primary_key: c.primary_key,
        });
    }
    Ok(LogicalPlan::CreateTable {
        def: TableDef {
            name: ct.name.clone(),
            schema: Schema::new(columns),
        },
        if_not_exists: ct.if_not_exists,
    })
}

fn bind_insert(ins: &InsertStmt, provider: &dyn SchemaProvider) -> Result<LogicalPlan> {
    let schema = provider
        .table_schema(&ins.table)?
        .ok_or_else(|| DbError::NotFound(format!("table {} does not exist", ins.table)))?;

    let target: Vec<String> = match &ins.columns {
        Some(c) => c.clone(),
        None => schema.names(),
    };
    if target.is_empty() {
        return Err(DbError::Bind("INSERT requires at least one column".into()));
    }

    let mut subset = Vec::with_capacity(target.len());
    for (i, name) in target.iter().enumerate() {
        if target[..i].iter().any(|x| x.eq_ignore_ascii_case(name)) {
            return Err(DbError::Bind(format!("duplicate column {}", name)));
        }
        let idx = schema
            .column_index(name)
            .ok_or_else(|| DbError::Bind(format!("unknown column {}", name)))?;
        subset.push(schema.columns[idx].clone());
    }

    // VALUES expressions may not reference table columns.
    let no_columns = empty_schema();
    let mut rows = Vec::with_capacity(ins.rows.len());
    for row in &ins.rows {
        if row.len() != target.len() {
            return Err(DbError::Bind(format!(
                "INSERT has {} target columns but {} values",
                target.len(),
                row.len()
            )));
        }
        let mut bound = Vec::with_capacity(row.len());
        for e in row {
            bound.push(bind_expr(e, &no_columns, None)?);
        }
        rows.push(bound);
    }

    let source = LogicalPlan::Values {
        rows,
        schema: Schema::new(subset),
    };
    Ok(LogicalPlan::Insert {
        table: ins.table.clone(),
        columns: target,
        source: Box::new(source),
    })
}

fn bind_update(u: &ast::UpdateStmt, provider: &dyn SchemaProvider) -> Result<LogicalPlan> {
    let schema = provider
        .table_schema(&u.table)?
        .ok_or_else(|| DbError::NotFound(format!("table {} does not exist", u.table)))?;

    let mut assignments = Vec::with_capacity(u.assignments.len());
    let mut seen: Vec<usize> = Vec::new();
    for a in &u.assignments {
        let idx = schema
            .column_index(&a.column)
            .ok_or_else(|| DbError::Bind(format!("unknown column {}", a.column)))?;
        if seen.contains(&idx) {
            return Err(DbError::Bind(format!(
                "column {} assigned twice",
                a.column
            )));
        }
        seen.push(idx);
        let value = bind_expr(&a.value, &schema, Some(&u.table))?;
        assignments.push((idx, value));
    }

    let predicate = match &u.selection {
        Some(e) => Some(bind_expr(e, &schema, Some(&u.table))?),
        None => None,
    };

    Ok(LogicalPlan::Update {
        table: u.table.clone(),
        schema,
        assignments,
        predicate,
    })
}

fn bind_delete(d: &ast::DeleteStmt, provider: &dyn SchemaProvider) -> Result<LogicalPlan> {
    let schema = provider
        .table_schema(&d.table)?
        .ok_or_else(|| DbError::NotFound(format!("table {} does not exist", d.table)))?;
    let predicate = match &d.selection {
        Some(e) => Some(bind_expr(e, &schema, Some(&d.table))?),
        None => None,
    };
    Ok(LogicalPlan::Delete {
        table: d.table.clone(),
        schema,
        predicate,
    })
}

// ---------------------------------------------------------------------------
// SELECT
// ---------------------------------------------------------------------------

fn collect_aggregate_calls(items: &[SelectItem]) -> Vec<Expr> {
    let mut out = Vec::new();
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_aggs(expr, &mut out);
        }
    }
    out
}

fn collect_aggs(e: &Expr, out: &mut Vec<Expr>) {
    if let Expr::Call { name, .. } = e {
        if ast::is_aggregate(name) {
            if !out.iter().any(|x| x == e) {
                out.push(e.clone());
            }
            return;
        }
    }
    match e {
        Expr::Binary { left, right, .. } => {
            collect_aggs(left, out);
            collect_aggs(right, out);
        }
        Expr::Unary { operand, .. } => collect_aggs(operand, out),
        Expr::IsNull { expr, .. } => collect_aggs(expr, out),
        Expr::InList { expr, list, .. } => {
            collect_aggs(expr, out);
            for x in list {
                collect_aggs(x, out);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_aggs(expr, out);
            collect_aggs(low, out);
            collect_aggs(high, out);
        }
        Expr::Call { args, .. } => {
            for a in args {
                collect_aggs(a, out);
            }
        }
        _ => {}
    }
}

fn contains_aggregate(e: &Expr) -> bool {
    let mut v = Vec::new();
    collect_aggs(e, &mut v);
    !v.is_empty()
}

/// Human-readable name for an output column derived from an AST expression.
fn ast_label(e: &Expr) -> String {
    match e {
        Expr::Literal(v) => v.to_string(),
        Expr::Column { name, .. } => name.clone(),
        Expr::Binary { op, left, right } => {
            format!("({} {} {})", ast_label(left), op.symbol(), ast_label(right))
        }
        Expr::Unary { op, operand } => match op {
            ast::UnOp::Neg => format!("-{}", ast_label(operand)),
            ast::UnOp::Not => format!("NOT {}", ast_label(operand)),
        },
        Expr::IsNull { expr, negated } => {
            format!("{} IS {}NULL", ast_label(expr), if *negated { "NOT " } else { "" })
        }
        Expr::InList { expr, list, negated } => {
            let items: Vec<String> = list.iter().map(ast_label).collect();
            format!(
                "{} {}IN ({})",
                ast_label(expr),
                if *negated { "NOT " } else { "" },
                items.join(", ")
            )
        }
        Expr::Between {
            expr, low, high, negated,
        } => format!(
            "{} {}BETWEEN {} AND {}",
            ast_label(expr),
            if *negated { "NOT " } else { "" },
            ast_label(low),
            ast_label(high)
        ),
        Expr::Call { name, args, .. } => {
            let items: Vec<String> = args.iter().map(ast_label).collect();
            format!("{}({})", name.to_lowercase(), items.join(", "))
        }
    }
}

fn agg_output_type(a: &AggExpr) -> DataType {
    match a.func {
        AggFunc::Count => DataType::Int,
        AggFunc::Avg => DataType::Float,
        AggFunc::Sum => match &a.arg {
            Some(e) if infer_type(e) == DataType::Float => DataType::Float,
            _ => DataType::Int,
        },
        AggFunc::Min | AggFunc::Max => a
            .arg
            .as_ref()
            .map(infer_type)
            .unwrap_or(DataType::Text),
    }
}

/// Bind an expression that lives *above* an Aggregate node: group-by
/// expressions and aggregate calls are replaced by columns of the aggregate's
/// output row.
fn bind_agg_context(
    expr: &Expr,
    groups: &[(Expr, BoundExpr)],
    agg_asts: &[Expr],
    aggs: &[AggExpr],
    table: Option<&str>,
) -> Result<BoundExpr> {
    // 1. aggregate call?
    if let Expr::Call { name, .. } = expr {
        if ast::is_aggregate(name) {
            let pos = agg_asts
                .iter()
                .position(|a| a == expr)
                .ok_or_else(|| DbError::Internal("aggregate not collected".into()))?;
            let agg = &aggs[pos];
            return Ok(BoundExpr::Column {
                index: groups.len() + pos,
                name: agg.label(),
                data_type: agg_output_type(agg),
            });
        }
    }

    // 2. exact group-by expression?
    if let Some((_, bound)) = groups.iter().find(|(g, _)| g == expr) {
        return Ok(bound.clone());
    }

    // 3. structural recursion
    match expr {
        Expr::Literal(v) => Ok(BoundExpr::Literal(v.clone())),
        Expr::Column { name, .. } => Err(DbError::Bind(format!(
            "column {} must appear in GROUP BY or be used in an aggregate",
            name
        ))),
        Expr::Binary { op, left, right } => Ok(BoundExpr::Binary {
            op: *op,
            left: Box::new(bind_agg_context(left, groups, agg_asts, aggs, table)?),
            right: Box::new(bind_agg_context(right, groups, agg_asts, aggs, table)?),
        }),
        Expr::Unary { op, operand } => Ok(BoundExpr::Unary {
            op: *op,
            operand: Box::new(bind_agg_context(operand, groups, agg_asts, aggs, table)?),
        }),
        Expr::IsNull { expr, negated } => Ok(BoundExpr::IsNull {
            expr: Box::new(bind_agg_context(expr, groups, agg_asts, aggs, table)?),
            negated: *negated,
        }),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let mut bound = Vec::with_capacity(list.len());
            for x in list {
                bound.push(bind_agg_context(x, groups, agg_asts, aggs, table)?);
            }
            Ok(BoundExpr::InList {
                expr: Box::new(bind_agg_context(expr, groups, agg_asts, aggs, table)?),
                list: bound,
                negated: *negated,
            })
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Ok(BoundExpr::Between {
            expr: Box::new(bind_agg_context(expr, groups, agg_asts, aggs, table)?),
            low: Box::new(bind_agg_context(low, groups, agg_asts, aggs, table)?),
            high: Box::new(bind_agg_context(high, groups, agg_asts, aggs, table)?),
            negated: *negated,
        }),
        Expr::Call {
            name,
            args,
            distinct,
        } => {
            if *distinct {
                return Err(DbError::Bind(
                    "DISTINCT is only supported inside aggregates".into(),
                ));
            }
            let arity = scalar_arity(name)
                .ok_or_else(|| DbError::Bind(format!("unknown function {}", name)))?;
            if args.len() != arity {
                return Err(DbError::Bind(format!(
                    "{} expects {} argument(s)",
                    name, arity
                )));
            }
            let mut bound = Vec::with_capacity(args.len());
            for a in args {
                bound.push(bind_agg_context(a, groups, agg_asts, aggs, table)?);
            }
            Ok(BoundExpr::Func {
                name: name.to_ascii_uppercase(),
                args: bound,
            })
        }
    }
}

fn bind_select(sel: &SelectStmt, provider: &dyn SchemaProvider) -> Result<LogicalPlan> {
    // ---- FROM + WHERE -----------------------------------------------------
    let table_name = sel.from.clone();
    let base_schema: Schema;
    let mut base_plan: LogicalPlan;

    match &sel.from {
        Some(t) => {
            let schema = provider
                .table_schema(t)?
                .ok_or_else(|| DbError::NotFound(format!("table {} does not exist", t)))?;
            base_schema = schema.clone();
            let mut plan = LogicalPlan::Scan {
                table: t.clone(),
                schema: schema.clone(),
            };
            if let Some(w) = &sel.selection {
                let predicate = bind_expr(w, &schema, Some(t))?;
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate,
                };
            }
            base_plan = plan;
        }
        None => {
            if sel.selection.is_some() {
                return Err(DbError::Bind("WHERE requires a FROM clause".into()));
            }
            base_schema = empty_schema();
            // A single empty row: makes `SELECT <constant expr>` work.
            base_plan = LogicalPlan::Values {
                rows: vec![Vec::new()],
                schema: empty_schema(),
            };
        }
    }

    // ---- HAVING guard (phase 1) ------------------------------------------
    if let Some(h) = &sel.having {
        if contains_aggregate(h) {
            return Err(DbError::Bind(
                "HAVING with aggregates is not supported yet".into(),
            ));
        }
    }

    let has_group = !sel.group_by.is_empty();
    let agg_calls = collect_aggregate_calls(&sel.items);
    let has_agg = !agg_calls.is_empty();

    if has_agg && sel.from.is_none() {
        return Err(DbError::Bind("aggregate functions require a FROM clause".into()));
    }
    if has_group && sel.from.is_none() {
        return Err(DbError::Bind("GROUP BY requires a FROM clause".into()));
    }

    // ---- projection expressions ------------------------------------------
    let mut exprs: Vec<(BoundExpr, String)> = Vec::new();
    let mut proj_input_schema: Schema;

    if has_group || has_agg {
        // group-by expressions
        let mut group_input: Vec<BoundExpr> = Vec::new();
        let mut group_subst: Vec<(Expr, BoundExpr)> = Vec::new();
        let mut agg_cols: Vec<ColumnDef> = Vec::new();

        for g in &sel.group_by {
            let bound = bind_expr(g, &base_schema, table_name.as_deref())?;
            let name = ast_label(g);
            let dt = infer_type(&bound);
            group_subst.push((
                g.clone(),
                BoundExpr::Column {
                    index: agg_cols.len(),
                    name: name.clone(),
                    data_type: dt,
                },
            ));
            agg_cols.push(ColumnDef {
                name,
                data_type: dt,
                nullable: true,
                primary_key: false,
            });
            group_input.push(bound);
        }

        // aggregates
        let mut aggs: Vec<AggExpr> = Vec::new();
        for call in &agg_calls {
            let (name, args, distinct) = match call {
                Expr::Call {
                    name,
                    args,
                    distinct,
                } => (name.clone(), args.clone(), *distinct),
                _ => unreachable!(),
            };
            let func = AggFunc::from_name(&name)
                .ok_or_else(|| DbError::Bind(format!("unknown aggregate {}", name)))?;
            if distinct {
                return Err(DbError::Bind(
                    "DISTINCT aggregates are not supported yet".into(),
                ));
            }
            let arg = if args.is_empty() {
                None
            } else if args.len() == 1 {
                match &args[0] {
                    Expr::Column { name, .. } if name == "*" => None,
                    other => Some(bind_expr(other, &base_schema, table_name.as_deref())?),
                }
            } else {
                return Err(DbError::Bind(format!(
                    "{} takes exactly one argument",
                    name
                )));
            };
            if func != AggFunc::Count && arg.is_none() {
                return Err(DbError::Bind(format!("{} does not accept *", name)));
            }
            let agg = AggExpr {
                func,
                arg,
                distinct: false,
            };
            let label = agg.label();
            let dt = agg_output_type(&agg);
            agg_cols.push(ColumnDef {
                name: label,
                data_type: dt,
                nullable: true,
                primary_key: false,
            });
            aggs.push(agg);
        }

        let agg_schema = Schema::new(agg_cols);
        let agg_plan = LogicalPlan::Aggregate {
            input: Box::new(base_plan),
            group_by: group_input,
            aggregates: aggs.clone(),
            schema: agg_schema.clone(),
        };
        base_plan = agg_plan;
        proj_input_schema = agg_schema;

        // bind each select item in the aggregate context
        for item in sel.items.iter() {
            match item {
                SelectItem::Wildcard | SelectItem::QualifiedWildcard { .. } => {
                    return Err(DbError::Bind(
                        "cannot use * with GROUP BY or aggregate functions".into(),
                    ))
                }
                SelectItem::Expr { expr, alias } => {
                    let bound = bind_agg_context(
                        expr,
                        &group_subst,
                        &agg_calls,
                        &aggs,
                        table_name.as_deref(),
                    )?;
                    let name = alias.clone().unwrap_or_else(|| ast_label(expr));
                    exprs.push((bound, name));
                }
            }
        }
    } else {
        // plain projection
        for (i, item) in sel.items.iter().enumerate() {
            match item {
                SelectItem::Wildcard => {
                    if sel.from.is_none() {
                        return Err(DbError::Bind("* requires a FROM clause".into()));
                    }
                    for (j, col) in base_schema.columns.iter().enumerate() {
                        exprs.push((
                            BoundExpr::Column {
                                index: j,
                                name: col.name.clone(),
                                data_type: col.data_type,
                            },
                            col.name.clone(),
                        ));
                    }
                }
                SelectItem::QualifiedWildcard { table } => {
                    match &table_name {
                        Some(t) if t.eq_ignore_ascii_case(table) => {}
                        _ => {
                            return Err(DbError::Bind(format!(
                                "unknown table prefix {}",
                                table
                            )))
                        }
                    }
                    for (j, col) in base_schema.columns.iter().enumerate() {
                        exprs.push((
                            BoundExpr::Column {
                                index: j,
                                name: col.name.clone(),
                                data_type: col.data_type,
                            },
                            col.name.clone(),
                        ));
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    let bound = bind_expr(expr, &base_schema, table_name.as_deref())?;
                    let name = match alias {
                        Some(a) => a.clone(),
                        None => match expr {
                            Expr::Column { name, .. } => name.clone(),
                            _ => format!("expr{}", i),
                        },
                    };
                    exprs.push((bound, name));
                }
            }
        }
        proj_input_schema = base_schema.clone();
    }

    // ---- ORDER BY ---------------------------------------------------------
    // Each key binds either to the projection's output ("above") or to its
    // input ("below"); mixing is rejected.
    let proj_schema = Schema::new(
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

    let mut above: Vec<(BoundExpr, bool)> = Vec::new();
    let mut below: Vec<(BoundExpr, bool)> = Vec::new();
    for ob in &sel.order_by {
        // ordinal reference: ORDER BY 1
        if let Expr::Literal(Value::Int(n)) = &ob.expr {
            let idx = (*n - 1) as usize;
            if *n < 1 || idx >= proj_schema.columns.len() {
                return Err(DbError::Bind(format!(
                    "ORDER BY position {} is out of range",
                    n
                )));
            }
            let col = &proj_schema.columns[idx];
            above.push((
                BoundExpr::Column {
                    index: idx,
                    name: col.name.clone(),
                    data_type: col.data_type,
                },
                ob.asc,
            ));
            continue;
        }

        if let Ok(e) = bind_expr(&ob.expr, &proj_schema, None) {
            above.push((e, ob.asc));
            continue;
        }
        if let Ok(e) = bind_expr(&ob.expr, &proj_input_schema, table_name.as_deref()) {
            below.push((e, ob.asc));
            continue;
        }
        return Err(DbError::Bind(format!(
            "ORDER BY expression {} is not in the SELECT list",
            ast_label(&ob.expr)
        )));
    }
    if !above.is_empty() && !below.is_empty() {
        return Err(DbError::Bind(
            "ORDER BY cannot mix output columns and input-only columns".into(),
        ));
    }

    // ---- assemble ---------------------------------------------------------
    let mut plan = if !below.is_empty() {
        LogicalPlan::Sort {
            input: Box::new(base_plan),
            keys: below,
        }
    } else {
        base_plan
    };
    plan = LogicalPlan::Project {
        input: Box::new(plan),
        exprs,
    };
    if !above.is_empty() {
        plan = LogicalPlan::Sort {
            input: Box::new(plan),
            keys: above,
        };
    }

    // ---- LIMIT / OFFSET ---------------------------------------------------
    let limit = match sel.limit {
        Some(l) if l < 0 => return Err(DbError::Bind("LIMIT must be non-negative".into())),
        Some(l) => Some(l as u64),
        None => None,
    };
    let offset = match sel.offset {
        Some(o) if o < 0 => return Err(DbError::Bind("OFFSET must be non-negative".into())),
        Some(o) => o as u64,
        None => 0,
    };
    if limit.is_some() || offset > 0 {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            limit,
            offset,
        };
    }

    Ok(plan)
}
