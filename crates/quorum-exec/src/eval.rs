//! Runtime expression evaluation with SQL three-valued logic.

use quorum_planner::BoundExpr;
use quorum_sql::ast::{BinOp, UnOp};
use quorum_types::{DbError, Result, Value};

pub fn eval(expr: &BoundExpr, row: &[Value]) -> Result<Value> {
    match expr {
        BoundExpr::Literal(v) => Ok(v.clone()),
        BoundExpr::Column { index, name, .. } => row.get(*index).cloned().ok_or_else(|| {
            DbError::Internal(format!("column {} (index {}) out of range", name, index))
        }),
        BoundExpr::Unary { op, operand } => {
            let v = eval(operand, row)?;
            match op {
                UnOp::Neg => match v {
                    Value::Null => Ok(Value::Null),
                    Value::Int(i) => i
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| DbError::Type("integer overflow in negation".into())),
                    Value::Float(f) => Ok(Value::Float(-f)),
                    other => Err(DbError::Type(format!(
                        "cannot negate {}",
                        other.type_name()
                    ))),
                },
                UnOp::Not => match v {
                    Value::Null => Ok(Value::Null),
                    Value::Bool(b) => Ok(Value::Bool(!b)),
                    other => Err(DbError::Type(format!(
                        "NOT requires BOOL, got {}",
                        other.type_name()
                    ))),
                },
            }
        }
        BoundExpr::IsNull { expr, negated } => {
            let v = eval(expr, row)?;
            Ok(Value::Bool(v.is_null() != *negated))
        }
        BoundExpr::InList {
            expr,
            list,
            negated,
        } => {
            let v = eval(expr, row)?;
            let mut saw_null = v.is_null();
            for item in list {
                let iv = eval(item, row)?;
                if iv.is_null() {
                    saw_null = true;
                    continue;
                }
                if v.is_null() {
                    continue;
                }
                match v.compare(&iv) {
                    Some(std::cmp::Ordering::Equal) => {
                        return Ok(Value::Bool(!*negated));
                    }
                    Some(_) => {}
                    None => {
                        return Err(DbError::Type(format!(
                            "cannot compare {} with {}",
                            v.type_name(),
                            iv.type_name()
                        )))
                    }
                }
            }
            if saw_null {
                Ok(Value::Null)
            } else {
                Ok(Value::Bool(*negated))
            }
        }
        BoundExpr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval(expr, row)?;
            let lo = eval(low, row)?;
            let hi = eval(high, row)?;
            if v.is_null() || lo.is_null() || hi.is_null() {
                return Ok(Value::Null);
            }
            let ge = compare_or_err(&v, &lo)?;
            let le = compare_or_err(&v, &hi)?;
            let in_range = ge != std::cmp::Ordering::Less && le != std::cmp::Ordering::Greater;
            Ok(Value::Bool(in_range != *negated))
        }
        BoundExpr::Binary { op, left, right } => match op {
            // short-circuit three-valued logic
            BinOp::And => {
                let l = eval(left, row)?;
                let lb = bool_or_null(&l)?;
                if lb == Some(false) {
                    return Ok(Value::Bool(false));
                }
                let r = eval(right, row)?;
                let rb = bool_or_null(&r)?;
                if rb == Some(false) {
                    return Ok(Value::Bool(false));
                }
                if lb == Some(true) && rb == Some(true) {
                    return Ok(Value::Bool(true));
                }
                Ok(Value::Null)
            }
            BinOp::Or => {
                let l = eval(left, row)?;
                let lb = bool_or_null(&l)?;
                if lb == Some(true) {
                    return Ok(Value::Bool(true));
                }
                let r = eval(right, row)?;
                let rb = bool_or_null(&r)?;
                if rb == Some(true) {
                    return Ok(Value::Bool(true));
                }
                if lb == Some(false) && rb == Some(false) {
                    return Ok(Value::Bool(false));
                }
                Ok(Value::Null)
            }
            op if op.is_comparison() => {
                let l = eval(left, row)?;
                let r = eval(right, row)?;
                if l.is_null() || r.is_null() {
                    return Ok(Value::Null);
                }
                let ord = compare_or_err(&l, &r)?;
                use std::cmp::Ordering::*;
                let b = match op {
                    BinOp::Eq => ord == Equal,
                    BinOp::Ne => ord != Equal,
                    BinOp::Lt => ord == Less,
                    BinOp::Le => ord != Greater,
                    BinOp::Gt => ord == Greater,
                    BinOp::Ge => ord != Less,
                    _ => unreachable!(),
                };
                Ok(Value::Bool(b))
            }
            op => {
                let l = eval(left, row)?;
                let r = eval(right, row)?;
                arithmetic(*op, l, r)
            }
        },
        BoundExpr::Func { name, args } => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, row)?);
            }
            eval_scalar(name, &vals)
        }
    }
}

fn bool_or_null(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(DbError::Type(format!(
            "logical operator requires BOOL, got {}",
            other.type_name()
        ))),
    }
}

fn compare_or_err(a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
    a.compare(b).ok_or_else(|| {
        DbError::Type(format!(
            "cannot compare {} with {}",
            a.type_name(),
            b.type_name()
        ))
    })
}

fn arithmetic(op: BinOp, l: Value, r: Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let is_float = matches!(l, Value::Float(_)) || matches!(r, Value::Float(_));
    if is_float {
        let a = l
            .as_f64()
            .ok_or_else(|| DbError::Type(format!("{} is not numeric", l.type_name())))?;
        let b = r
            .as_f64()
            .ok_or_else(|| DbError::Type(format!("{} is not numeric", r.type_name())))?;
        let out = match op {
            BinOp::Add => a + b,
            BinOp::Sub => a - b,
            BinOp::Mul => a * b,
            BinOp::Div => {
                if b == 0.0 {
                    return Err(DbError::Type("division by zero".into()));
                }
                a / b
            }
            BinOp::Mod => {
                if b == 0.0 {
                    return Err(DbError::Type("division by zero".into()));
                }
                a % b
            }
            _ => unreachable!(),
        };
        Ok(Value::Float(out))
    } else {
        let a = match l {
            Value::Int(i) => i,
            other => {
                return Err(DbError::Type(format!(
                    "{} is not numeric",
                    other.type_name()
                )))
            }
        };
        let b = match r {
            Value::Int(i) => i,
            other => {
                return Err(DbError::Type(format!(
                    "{} is not numeric",
                    other.type_name()
                )))
            }
        };
        let out = match op {
            BinOp::Add => a.checked_add(b),
            BinOp::Sub => a.checked_sub(b),
            BinOp::Mul => a.checked_mul(b),
            BinOp::Div => {
                if b == 0 {
                    return Err(DbError::Type("division by zero".into()));
                }
                a.checked_div(b)
            }
            BinOp::Mod => {
                if b == 0 {
                    return Err(DbError::Type("division by zero".into()));
                }
                a.checked_rem(b)
            }
            _ => unreachable!(),
        };
        out.map(Value::Int)
            .ok_or_else(|| DbError::Type("integer overflow".into()))
    }
}

fn eval_scalar(name: &str, args: &[Value]) -> Result<Value> {
    match name {
        "UPPER" | "LOWER" => match &args[0] {
            Value::Null => Ok(Value::Null),
            Value::Text(s) => Ok(Value::Text(if name == "UPPER" {
                s.to_uppercase()
            } else {
                s.to_lowercase()
            })),
            other => Err(DbError::Type(format!(
                "{} requires TEXT, got {}",
                name,
                other.type_name()
            ))),
        },
        "LENGTH" => match &args[0] {
            Value::Null => Ok(Value::Null),
            Value::Text(s) => Ok(Value::Int(s.chars().count() as i64)),
            other => Err(DbError::Type(format!(
                "LENGTH requires TEXT, got {}",
                other.type_name()
            ))),
        },
        "ABS" => match &args[0] {
            Value::Null => Ok(Value::Null),
            Value::Int(i) => i
                .checked_abs()
                .map(Value::Int)
                .ok_or_else(|| DbError::Type("integer overflow in ABS".into())),
            Value::Float(f) => Ok(Value::Float(f.abs())),
            other => Err(DbError::Type(format!(
                "ABS requires a number, got {}",
                other.type_name()
            ))),
        },
        other => Err(DbError::Internal(format!("unknown function {}", other))),
    }
}
