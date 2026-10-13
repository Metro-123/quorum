//! End-to-end SQL tests: parse -> bind -> plan -> execute over MVCC storage.

use quorum_exec::Database;
use quorum_types::Value;

fn db() -> Database {
    Database::in_memory().unwrap()
}

fn setup(db: &Database) {
    db.execute(
        "CREATE TABLE users (
            id INT PRIMARY KEY,
            name TEXT NOT NULL,
            score FLOAT,
            active BOOL
         )",
    )
    .unwrap();
    db.execute(
        "INSERT INTO users (id, name, score, active) VALUES
            (1, 'alice', 9.5, true),
            (2, 'bob', 7.0, false),
            (3, 'carol', 8.25, true),
            (4, 'dave', NULL, false)",
    )
    .unwrap();
}

fn one(db: &Database, sql: &str) -> quorum_exec::ResultSet {
    db.execute(sql).unwrap().into_iter().next().unwrap()
}

#[test]
fn create_insert_select() {
    let db = db();
    setup(&db);

    let rs = one(&db, "SELECT id, name, score FROM users WHERE id = 2");
    assert_eq!(rs.columns, vec!["id", "name", "score"]);
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][1], Value::Text("bob".into()));
    assert_eq!(rs.rows[0][2], Value::Float(7.0));

    let rs = one(&db, "SELECT * FROM users");
    assert_eq!(rs.rows.len(), 4);
    assert_eq!(rs.columns.len(), 4);
}

#[test]
fn where_boolean_logic_and_nulls() {
    let db = db();
    setup(&db);

    // score is NULL for dave -> comparisons are NULL -> not true
    let rs = one(&db, "SELECT id FROM users WHERE score > 8.0");
    assert_eq!(rs.rows.len(), 2); // alice 9.5, carol 8.25

    let rs = one(&db, "SELECT id FROM users WHERE score IS NULL");
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], Value::Int(4));

    let rs = one(&db, "SELECT id FROM users WHERE active AND score > 8.0");
    assert_eq!(rs.rows.len(), 2);

    let rs = one(&db, "SELECT id FROM users WHERE active OR id = 4");
    assert_eq!(rs.rows.len(), 3); // 1,3 active; 4 by id

    let rs = one(&db, "SELECT id FROM users WHERE NOT active");
    assert_eq!(rs.rows.len(), 2); // bob, dave
}

#[test]
fn in_between_like_patterns() {
    let db = db();
    setup(&db);

    let rs = one(&db, "SELECT name FROM users WHERE id IN (1, 3, 99)");
    assert_eq!(rs.rows.len(), 2);

    let rs = one(&db, "SELECT name FROM users WHERE id NOT IN (1, 2)");
    assert_eq!(rs.rows.len(), 2);

    let rs = one(&db, "SELECT name FROM users WHERE score BETWEEN 7.0 AND 8.5");
    assert_eq!(rs.rows.len(), 2); // bob 7.0, carol 8.25
}

#[test]
fn order_by_and_limit() {
    let db = db();
    setup(&db);

    let rs = one(&db, "SELECT id FROM users ORDER BY id DESC");
    let ids: Vec<i64> = rs
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Int(i) => i,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![4, 3, 2, 1]);

    let rs = one(&db, "SELECT id FROM users ORDER BY id LIMIT 2 OFFSET 1");
    let ids: Vec<i64> = rs
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Int(i) => i,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![2, 3]);

    // ORDER BY an input column that is not projected
    let rs = one(&db, "SELECT name FROM users ORDER BY score DESC");
    // NULLs sort last on DESC -> alice, carol, bob, dave
    assert_eq!(rs.rows[0][0], Value::Text("alice".into()));

    // ORDER BY alias
    let rs = one(&db, "SELECT name AS n FROM users ORDER BY n LIMIT 1");
    assert_eq!(rs.rows[0][0], Value::Text("alice".into()));
}

#[test]
fn update_and_delete() {
    let db = db();
    setup(&db);

    let rs = one(&db, "UPDATE users SET score = score + 1.0 WHERE active");
    assert_eq!(rs.message, "UPDATE 2");

    let rs = one(&db, "SELECT score FROM users WHERE id = 1");
    assert_eq!(rs.rows[0][0], Value::Float(10.5));

    let rs = one(&db, "DELETE FROM users WHERE active = false");
    assert_eq!(rs.message, "DELETE 2");

    let rs = one(&db, "SELECT COUNT(*) FROM users");
    assert_eq!(rs.rows[0][0], Value::Int(2));
}

#[test]
fn aggregates_and_group_by() {
    let db = db();
    db.execute("CREATE TABLE emp (dept TEXT, salary INT)").unwrap();
    db.execute(
        "INSERT INTO emp (dept, salary) VALUES
            ('eng', 100), ('eng', 200), ('sales', 150), ('sales', 50), ('hr', NULL)",
    )
    .unwrap();

    let rs = one(&db, "SELECT COUNT(*) FROM emp");
    assert_eq!(rs.rows[0][0], Value::Int(5));

    let rs = one(&db, "SELECT COUNT(salary) FROM emp");
    assert_eq!(rs.rows[0][0], Value::Int(4));

    let rs = one(&db, "SELECT SUM(salary), MIN(salary), MAX(salary), AVG(salary) FROM emp");
    assert_eq!(rs.rows[0][0], Value::Int(500));
    assert_eq!(rs.rows[0][1], Value::Int(50));
    assert_eq!(rs.rows[0][2], Value::Int(200));
    assert_eq!(rs.rows[0][3], Value::Float(125.0));

    let rs = one(&db, "SELECT dept, SUM(salary) FROM emp GROUP BY dept ORDER BY dept");
    assert_eq!(rs.rows.len(), 3);
    assert_eq!(rs.rows[0][0], Value::Text("eng".into()));
    assert_eq!(rs.rows[0][1], Value::Int(300));
    // hr has only a NULL salary -> SUM is NULL
    assert_eq!(rs.rows[1][0], Value::Text("hr".into()));
    assert_eq!(rs.rows[1][1], Value::Null);

    // aggregate over empty table
    db.execute("CREATE TABLE empty (x INT)").unwrap();
    let rs = one(&db, "SELECT COUNT(*), SUM(x) FROM empty");
    assert_eq!(rs.rows[0][0], Value::Int(0));
    assert_eq!(rs.rows[0][1], Value::Null);
}

#[test]
fn expressions_and_scalar_functions() {
    let db = db();
    setup(&db);

    let rs = one(&db, "SELECT 1 + 2 * 3");
    assert_eq!(rs.rows[0][0], Value::Int(7));

    let rs = one(&db, "SELECT UPPER(name), LENGTH(name) FROM users WHERE id = 1");
    assert_eq!(rs.rows[0][0], Value::Text("ALICE".into()));
    assert_eq!(rs.rows[0][1], Value::Int(5));

    let rs = one(&db, "SELECT ABS(-5), LOWER('ABC')");
    assert_eq!(rs.rows[0][0], Value::Int(5));
    assert_eq!(rs.rows[0][1], Value::Text("abc".into()));

    let rs = one(&db, "SELECT id * 10 AS x FROM users WHERE id = 3");
    assert_eq!(rs.rows[0][0], Value::Int(30));
}

#[test]
fn constraint_enforcement() {
    let db = db();
    setup(&db);

    // NOT NULL
    let err = db
        .execute("INSERT INTO users (id, name) VALUES (10, NULL)")
        .unwrap_err();
    assert!(
        format!("{}", err).contains("cannot be null"),
        "unexpected error: {}",
        err
    );

    // primary key not null
    let err = db
        .execute("INSERT INTO users (name) VALUES ('x')")
        .unwrap_err();
    assert!(format!("{}", err).contains("cannot be null"));

    // type mismatch
    let err = db
        .execute("INSERT INTO users (id, name, score) VALUES (11, 'x', 'not a number')")
        .unwrap_err();
    assert!(format!("{}", err).contains("cannot store"), "got {}", err);

    // after failed inserts the table is unchanged
    let rs = one(&db, "SELECT COUNT(*) FROM users");
    assert_eq!(rs.rows[0][0], Value::Int(4));
}

#[test]
fn binding_errors_are_clear() {
    let db = db();
    setup(&db);

    let err = db.execute("SELECT * FROM nope").unwrap_err();
    assert!(format!("{}", err).contains("does not exist"), "got {}", err);

    let err = db.execute("SELECT nope FROM users").unwrap_err();
    assert!(format!("{}", err).contains("unknown column"), "got {}", err);

    let err = db.execute("SELECT id FROM users WHERE id =").unwrap_err();
    assert!(format!("{}", err).contains("parse"), "got {}", err);

    let err = db
        .execute("SELECT dept FROM emp GROUP BY other")
        .unwrap_err();
    assert!(format!("{}", err).contains("does not exist"), "got {}", err);

    // aggregates require FROM
    let err = db.execute("SELECT COUNT(*)").unwrap_err();
    assert!(format!("{}", err).contains("FROM"), "got {}", err);
}

#[test]
fn division_by_zero() {
    let db = db();
    setup(&db);
    let err = db.execute("SELECT 1 / 0").unwrap_err();
    assert!(format!("{}", err).contains("division by zero"), "got {}", err);
    let err = db.execute("SELECT id / 0 FROM users").unwrap_err();
    assert!(format!("{}", err).contains("division by zero"), "got {}", err);
}

#[test]
fn multi_statement_script_and_drop() {
    let db = db();
    let results = db
        .execute(
            "CREATE TABLE t (a INT);
             INSERT INTO t (a) VALUES (1), (2), (3);
             SELECT a FROM t WHERE a > 1;
             DROP TABLE t;",
        )
        .unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(results[0].message, "CREATE TABLE");
    assert_eq!(results[1].message, "INSERT 3");
    assert_eq!(results[2].rows.len(), 2);
    assert_eq!(results[3].message, "DROP TABLE");

    // table is gone
    assert!(db.execute("SELECT * FROM t").is_err());
}

#[test]
fn create_table_if_not_exists_and_drop_if_exists() {
    let db = db();
    let rs = one(&db, "CREATE TABLE t (a INT)");
    assert_eq!(rs.message, "CREATE TABLE");
    let rs = one(&db, "CREATE TABLE IF NOT EXISTS t (a INT)");
    assert_eq!(rs.message, "CREATE TABLE (already exists)");
    let rs = one(&db, "DROP TABLE IF EXISTS missing");
    assert_eq!(rs.message, "DROP TABLE (did not exist)");
}

#[test]
fn persistence_across_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "quorum-exec-persist-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    {
        let db = Database::open(&dir).unwrap();
        setup(&db);
        db.execute("UPDATE users SET score = 1.0 WHERE id = 1").unwrap();
    }
    {
        let db = Database::open(&dir).unwrap();
        let rs = one(&db, "SELECT name, score FROM users WHERE id = 1");
        assert_eq!(rs.rows[0][0], Value::Text("alice".into()));
        assert_eq!(rs.rows[0][1], Value::Float(1.0));
        let rs = one(&db, "SELECT COUNT(*) FROM users");
        assert_eq!(rs.rows[0][0], Value::Int(4));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
