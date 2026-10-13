//! End-to-end test: a real node over TCP, driven by the wire protocol.

use quorum_exec::Database;
use quorum_proto::{request, ClientRequest, ClientResponse};
use quorum_types::Value;
use std::net::TcpStream;

fn expect_results(resp: ClientResponse) -> Vec<quorum_proto::QueryResult> {
    match resp {
        ClientResponse::Results { results } => results,
        other => panic!("expected results, got {:?}", other),
    }
}

#[test]
fn sql_round_trip_over_tcp() {
    let db = Database::in_memory().unwrap();
    let node = quorum_node::start_with_db("127.0.0.1:0", db).unwrap();
    let mut stream = TcpStream::connect(node.addr()).unwrap();

    // ping
    match request(&mut stream, &ClientRequest::Ping).unwrap() {
        ClientResponse::Pong => {}
        other => panic!("expected pong, got {:?}", other),
    }

    // create + insert + select in one script
    let resp = request(
        &mut stream,
        &ClientRequest::Execute {
            sql: "CREATE TABLE t (id INT PRIMARY KEY, name TEXT); \
                  INSERT INTO t (id, name) VALUES (1, 'alice'), (2, 'bob'); \
                  SELECT id, name FROM t ORDER BY id;"
                .to_string(),
        },
    )
    .unwrap();
    let results = expect_results(resp);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].message, "CREATE TABLE");
    assert_eq!(results[1].message, "INSERT 2");
    assert_eq!(results[2].rows.len(), 2);
    assert_eq!(results[2].columns, vec!["id", "name"]);
    assert_eq!(results[2].rows[1][1], Value::Text("bob".into()));

    // errors come back as Error responses, connection stays usable
    let resp = request(
        &mut stream,
        &ClientRequest::Execute {
            sql: "SELECT * FROM missing_table".to_string(),
        },
    )
    .unwrap();
    match resp {
        ClientResponse::Error { message } => {
            assert!(message.contains("does not exist"), "got {}", message)
        }
        other => panic!("expected error, got {:?}", other),
    }

    // still works after the error
    let resp = request(
        &mut stream,
        &ClientRequest::Execute {
            sql: "SELECT COUNT(*) FROM t".to_string(),
        },
    )
    .unwrap();
    let results = expect_results(resp);
    assert_eq!(results[0].rows[0][0], Value::Int(2));
}

#[test]
fn two_clients_share_the_database() {
    let db = Database::in_memory().unwrap();
    let node = quorum_node::start_with_db("127.0.0.1:0", db).unwrap();

    let mut a = TcpStream::connect(node.addr()).unwrap();
    let mut b = TcpStream::connect(node.addr()).unwrap();

    // client A creates and writes
    expect_results(
        request(
            &mut a,
            &ClientRequest::Execute {
                sql: "CREATE TABLE kv (k TEXT, v TEXT); INSERT INTO kv (k, v) VALUES ('x', '1');"
                    .to_string(),
            },
        )
        .unwrap(),
    );

    // client B reads it
    let results = expect_results(
        request(
            &mut b,
            &ClientRequest::Execute {
                sql: "SELECT v FROM kv WHERE k = 'x'".to_string(),
            },
        )
        .unwrap(),
    );
    assert_eq!(results[0].rows[0][0], Value::Text("1".into()));
}
