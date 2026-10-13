use quorum_sql::ast::*;
use quorum_sql::parse;
use quorum_types::{DataType, Value};

#[test]
fn create_table_full() {
    let stmts = parse(
        "CREATE TABLE IF NOT EXISTS users (
            id INT PRIMARY KEY,
            name TEXT NOT NULL,
            score FLOAT,
            active BOOL
         )",
    )
    .unwrap();
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Statement::CreateTable(ct) => {
            assert_eq!(ct.name, "users");
            assert!(ct.if_not_exists);
            assert_eq!(ct.columns.len(), 4);
            assert_eq!(ct.columns[0].data_type, DataType::Int);
            assert!(ct.columns[0].primary_key);
            assert!(!ct.columns[0].nullable);
            assert!(!ct.columns[1].primary_key);
            assert!(!ct.columns[1].nullable); // NOT NULL
            assert!(ct.columns[2].nullable);
        }
        other => panic!("wrong statement: {:?}", other),
    }
}

#[test]
fn insert_multiple_rows_with_columns() {
    let stmts = parse(
        "INSERT INTO users (id, name) VALUES (1, 'alice'), (2, 'bob''s')",
    )
    .unwrap();
    match &stmts[0] {
        Statement::Insert(ins) => {
            assert_eq!(ins.table, "users");
            assert_eq!(ins.columns.as_deref(), Some(["id".to_string(), "name".to_string()].as_slice()));
            assert_eq!(ins.rows.len(), 2);
            assert_eq!(ins.rows[0][0], Expr::Literal(Value::Int(1)));
            assert_eq!(
                ins.rows[1][1],
                Expr::Literal(Value::Text("bob's".into()))
            );
        }
        other => panic!("wrong statement: {:?}", other),
    }
}

#[test]
fn select_with_where_order_limit() {
    let stmts = parse(
        "SELECT id, name AS n FROM users WHERE score >= 3.5 AND name IN ('a', 'b') \
         ORDER BY id DESC LIMIT 10 OFFSET 5",
    )
    .unwrap();
    match &stmts[0] {
        Statement::Select(sel) => {
            assert_eq!(sel.items.len(), 2);
            assert_eq!(sel.from.as_deref(), Some("users"));
            let w = sel.selection.as_ref().unwrap();
            assert!(matches!(w, Expr::Binary { op: BinOp::And, .. }));
            assert_eq!(sel.order_by.len(), 1);
            assert!(!sel.order_by[0].asc);
            assert_eq!(sel.limit, Some(10));
            assert_eq!(sel.offset, Some(5));
        }
        other => panic!("wrong statement: {:?}", other),
    }
}

#[test]
fn expression_precedence() {
    let stmts = parse("SELECT 1 + 2 * 3").unwrap();
    match &stmts[0] {
        Statement::Select(sel) => {
            let e = match &sel.items[0] {
                SelectItem::Expr { expr, .. } => expr,
                _ => panic!(),
            };
            // expect 1 + (2 * 3)
            assert!(matches!(
                e,
                Expr::Binary {
                    op: BinOp::Add,
                    ..
                }
            ));
            if let Expr::Binary { right, .. } = e {
                assert!(matches!(
                    right.as_ref(),
                    Expr::Binary {
                        op: BinOp::Mul,
                        ..
                    }
                ));
            }
        }
        other => panic!("wrong statement: {:?}", other),
    }
}

#[test]
fn between_and_not_null() {
    let stmts = parse("SELECT * FROM t WHERE age BETWEEN 18 AND 30 AND email IS NOT NULL")
        .unwrap();
    match &stmts[0] {
        Statement::Select(sel) => {
            let w = sel.selection.as_ref().unwrap();
            assert!(matches!(w, Expr::Binary { op: BinOp::And, .. }));
        }
        other => panic!("wrong statement: {:?}", other),
    }
}

#[test]
fn update_and_delete() {
    let stmts = parse("UPDATE t SET a = 1, b = b + 1 WHERE id = 3").unwrap();
    match &stmts[0] {
        Statement::Update(u) => {
            assert_eq!(u.assignments.len(), 2);
            assert!(u.selection.is_some());
        }
        other => panic!("wrong statement: {:?}", other),
    }
    let stmts = parse("DELETE FROM t WHERE id = 3").unwrap();
    assert!(matches!(stmts[0], Statement::Delete(_)));
    let stmts = parse("DELETE FROM t").unwrap();
    match &stmts[0] {
        Statement::Delete(d) => assert!(d.selection.is_none()),
        other => panic!("wrong statement: {:?}", other),
    }
}

#[test]
fn aggregates_and_group_by() {
    let stmts =
        parse("SELECT dept, COUNT(*), SUM(salary) FROM emp GROUP BY dept ORDER BY dept")
            .unwrap();
    match &stmts[0] {
        Statement::Select(sel) => {
            assert_eq!(sel.items.len(), 3);
            assert_eq!(sel.group_by.len(), 1);
        }
        other => panic!("wrong statement: {:?}", other),
    }
}

#[test]
fn comments_and_case_insensitivity() {
    let stmts = parse("-- a comment\nselect * from T /* block */ where a = 1;").unwrap();
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Statement::Select(sel) => assert_eq!(sel.from.as_deref(), Some("T")),
        other => panic!("wrong statement: {:?}", other),
    }
}

#[test]
fn rejects_unsupported() {
    assert!(parse("CREATE INDEX ON t(a)").is_err());
    assert!(parse("SELECT DISTINCT a FROM t").is_err());
    assert!(parse("SELECT * FROM a JOIN b ON a.id = b.id").is_err());
    assert!(parse("SELECT * FROM t WHERE").is_err());
}
