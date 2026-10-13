//! The quorum node: a TCP server that executes SQL from clients.
//!
//! Phase 1 is a single-node server in front of the MVCC storage engine.
//! Phase 3 replaces the direct `Database::execute` with a Raft-replicated
//! state machine (the `quorum-raft` crate is already in the workspace).

use quorum_exec::{Database, ResultSet};
use quorum_proto::{self, ClientRequest, ClientResponse, QueryResult};
use quorum_types::Result;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub listen: String,
    pub data_dir: Option<PathBuf>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            listen: "127.0.0.1:7300".to_string(),
            data_dir: None,
        }
    }
}

pub struct NodeHandle {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
}

impl NodeHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signal the accept loop to stop. The listener wakes on the next
    /// connection attempt; this is best-effort teardown.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

pub fn start(config: NodeConfig) -> Result<NodeHandle> {
    let db = match &config.data_dir {
        Some(dir) => Database::open(dir)?,
        None => Database::in_memory()?,
    };
    start_with_db(&config.listen, db)
}

/// Start a node on `listen` (e.g. `127.0.0.1:0` for an OS-assigned port).
pub fn start_with_db(listen: &str, db: Database) -> Result<NodeHandle> {
    let listener = TcpListener::bind(listen)?;
    let addr = listener.local_addr()?;
    let db = Arc::new(db);
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if flag.load(Ordering::SeqCst) {
                break;
            }
            match stream {
                Ok(stream) => {
                    let db = Arc::clone(&db);
                    std::thread::spawn(move || {
                        let _ = handle_connection(stream, db);
                    });
                }
                Err(_) => break,
            }
        }
    });

    Ok(NodeHandle { addr, shutdown })
}

fn to_query_result(rs: ResultSet) -> QueryResult {
    QueryResult {
        columns: rs.columns,
        rows: rs.rows,
        message: rs.message,
    }
}

fn handle_connection(mut stream: TcpStream, db: Arc<Database>) -> Result<()> {
    loop {
        let payload = match quorum_proto::read_frame(&mut stream) {
            Ok(p) => p,
            Err(_) => return Ok(()), // client disconnected
        };
        let req = match quorum_proto::decode_request(&payload) {
            Ok(r) => r,
            Err(e) => {
                let resp = ClientResponse::Error {
                    message: e.to_string(),
                };
                quorum_proto::write_frame(&mut stream, &quorum_proto::encode_response(&resp))?;
                return Ok(());
            }
        };

        let resp = match req {
            ClientRequest::Ping => ClientResponse::Pong,
            ClientRequest::Execute { sql } => match db.execute(&sql) {
                Ok(results) => ClientResponse::Results {
                    results: results.into_iter().map(to_query_result).collect(),
                },
                Err(e) => ClientResponse::Error {
                    message: e.to_string(),
                },
            },
        };
        quorum_proto::write_frame(&mut stream, &quorum_proto::encode_response(&resp))?;
    }
}
