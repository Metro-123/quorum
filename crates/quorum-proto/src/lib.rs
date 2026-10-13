//! Client wire protocol and framing.
//!
//! Frame: `"QRM1" || u32 payload_len || payload`.
//! Payloads are encoded with the dependency-free codec from `quorum-types`.

use quorum_types::codec::{decode_value, encode_value, put_str, put_u32, put_u8, Reader};
use quorum_types::{DbError, Result, Value};
use std::io::{Read, Write};

pub const MAGIC: [u8; 4] = *b"QRM1";
const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum ClientRequest {
    /// Execute a SQL script.
    Execute { sql: String },
    /// Liveness check.
    Ping,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum ClientResponse {
    Results { results: Vec<QueryResult> },
    Pong,
    Error { message: String },
}

// ---------------------------------------------------------------------------
// framing
// ---------------------------------------------------------------------------

pub fn write_frame<W: Write>(stream: &mut W, payload: &[u8]) -> Result<()> {
    let mut header = Vec::with_capacity(8);
    header.extend_from_slice(&MAGIC);
    header.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

pub fn read_frame<R: Read>(stream: &mut R) -> Result<Vec<u8>> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header)?;
    if header[0..4] != MAGIC {
        return Err(DbError::Network("bad frame magic".into()));
    }
    let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if len > MAX_FRAME {
        return Err(DbError::Network(format!("frame too large: {}", len)));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------

pub fn encode_request(req: &ClientRequest) -> Vec<u8> {
    let mut out = Vec::new();
    match req {
        ClientRequest::Execute { sql } => {
            put_u8(&mut out, 1);
            put_str(&mut out, sql);
        }
        ClientRequest::Ping => put_u8(&mut out, 2),
    }
    out
}

pub fn decode_request(payload: &[u8]) -> Result<ClientRequest> {
    let mut r = Reader::new(payload);
    match r.u8()? {
        1 => Ok(ClientRequest::Execute { sql: r.str()? }),
        2 => Ok(ClientRequest::Ping),
        t => Err(DbError::Network(format!("unknown request tag {}", t))),
    }
}

// ---------------------------------------------------------------------------
// responses
// ---------------------------------------------------------------------------

pub fn encode_response(resp: &ClientResponse) -> Vec<u8> {
    let mut out = Vec::new();
    match resp {
        ClientResponse::Results { results } => {
            put_u8(&mut out, 1);
            put_u32(&mut out, results.len() as u32);
            for qr in results {
                put_str(&mut out, &qr.message);
                put_u32(&mut out, qr.columns.len() as u32);
                for c in &qr.columns {
                    put_str(&mut out, c);
                }
                put_u32(&mut out, qr.rows.len() as u32);
                for row in &qr.rows {
                    put_u32(&mut out, row.len() as u32);
                    for v in row {
                        encode_value(v, &mut out);
                    }
                }
            }
        }
        ClientResponse::Pong => put_u8(&mut out, 2),
        ClientResponse::Error { message } => {
            put_u8(&mut out, 3);
            put_str(&mut out, message);
        }
    }
    out
}

pub fn decode_response(payload: &[u8]) -> Result<ClientResponse> {
    let mut r = Reader::new(payload);
    match r.u8()? {
        1 => {
            let n = r.u32()? as usize;
            let mut results = Vec::with_capacity(n);
            for _ in 0..n {
                let message = r.str()?;
                let ncols = r.u32()? as usize;
                let mut columns = Vec::with_capacity(ncols);
                for _ in 0..ncols {
                    columns.push(r.str()?);
                }
                let nrows = r.u32()? as usize;
                let mut rows = Vec::with_capacity(nrows);
                for _ in 0..nrows {
                    let nvals = r.u32()? as usize;
                    let mut row = Vec::with_capacity(nvals);
                    for _ in 0..nvals {
                        row.push(decode_value(&mut r)?);
                    }
                    rows.push(row);
                }
                results.push(QueryResult {
                    columns,
                    rows,
                    message,
                });
            }
            Ok(ClientResponse::Results { results })
        }
        2 => Ok(ClientResponse::Pong),
        3 => Ok(ClientResponse::Error { message: r.str()? }),
        t => Err(DbError::Network(format!("unknown response tag {}", t))),
    }
}

// ---------------------------------------------------------------------------
// blocking client helper (used by the CLI and tests)
// ---------------------------------------------------------------------------

/// Send one request and read one response on a synchronous stream.
pub fn request<S: Read + Write>(stream: &mut S, req: &ClientRequest) -> Result<ClientResponse> {
    let payload = encode_request(req);
    write_frame(stream, &payload)?;
    let resp = read_frame(stream)?;
    decode_response(&resp)
}
