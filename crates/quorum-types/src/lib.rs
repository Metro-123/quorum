//! Shared value, schema, and error types used by every quorum crate.

use std::fmt;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int,
    Float,
    Bool,
    Text,
}

impl DataType {
    pub fn name(&self) -> &'static str {
        match self {
            DataType::Int => "INT",
            DataType::Float => "FLOAT",
            DataType::Bool => "BOOL",
            DataType::Text => "TEXT",
        }
    }

    /// Parse a SQL type name. Accepts common synonyms.
    pub fn from_name(name: &str) -> Option<DataType> {
        match name.to_ascii_uppercase().as_str() {
            "INT" | "INTEGER" | "BIGINT" | "SERIAL" => Some(DataType::Int),
            "FLOAT" | "REAL" | "DOUBLE" | "DOUBLE PRECISION" => Some(DataType::Float),
            "BOOL" | "BOOLEAN" => Some(DataType::Bool),
            "TEXT" | "STRING" | "VARCHAR" | "CHAR" => Some(DataType::Text),
            _ => None,
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
}

impl Value {
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Int(_) => Some(DataType::Int),
            Value::Float(_) => Some(DataType::Float),
            Value::Bool(_) => Some(DataType::Bool),
            Value::Text(_) => Some(DataType::Text),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Int(_) => "INT",
            Value::Float(_) => "FLOAT",
            Value::Bool(_) => "BOOL",
            Value::Text(_) => "TEXT",
        }
    }

    /// Coerce this value into the declared column type (insert/update path).
    /// NULL always coerces; int widens to float; everything else must match.
    pub fn coerce(&self, target: DataType) -> Result<Value> {
        if self.is_null() {
            return Ok(Value::Null);
        }
        match (self, target) {
            (Value::Int(_), DataType::Int) => Ok(self.clone()),
            (Value::Int(i), DataType::Float) => Ok(Value::Float(*i as f64)),
            (Value::Float(_), DataType::Float) => Ok(self.clone()),
            (Value::Bool(_), DataType::Bool) => Ok(self.clone()),
            (Value::Text(_), DataType::Text) => Ok(self.clone()),
            (v, t) => Err(DbError::Type(format!(
                "cannot store {} value in {} column",
                v.type_name(),
                t.name()
            ))),
        }
    }

    /// Three-valued truth for WHERE clauses: NULL is not true.
    pub fn truthy(&self) -> Result<bool> {
        match self {
            Value::Bool(b) => Ok(*b),
            Value::Null => Ok(false),
            v => Err(DbError::Type(format!(
                "condition must be BOOL, got {}",
                v.type_name()
            ))),
        }
    }

    /// SQL comparison: NULL is not comparable with anything (returns None),
    /// INT and FLOAT compare numerically.
    pub fn compare(&self, other: &Value) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering;
        match (self, other) {
            (Value::Null, _) | (_, Value::Null) => None,
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Int(i) => write!(f, "{}", i),
            Value::Float(x) => write!(f, "{}", x),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Text(s) => write!(f, "{}", s),
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub primary_key: bool,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        ColumnDef {
            name: name.into(),
            data_type,
            nullable: true,
            primary_key: false,
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub columns: Vec<ColumnDef>,
}

impl Schema {
    pub fn new(columns: Vec<ColumnDef>) -> Self {
        Schema { columns }
    }

    /// Case-insensitive column lookup (SQL identifiers are case-insensitive here).
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.column_index(name).map(|i| &self.columns[i])
    }

    pub fn names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }

    /// Sub-schema consisting of the given column indices, in order given.
    pub fn project(&self, indices: &[usize]) -> Result<Schema> {
        let mut columns = Vec::with_capacity(indices.len());
        for &i in indices {
            let col = self.columns.get(i).ok_or_else(|| {
                DbError::Internal(format!("column index {} out of range", i))
            })?;
            columns.push(col.clone());
        }
        Ok(Schema::new(columns))
    }

    /// Append a column (used to build aggregate/join output schemas).
    pub fn with_column(&self, col: ColumnDef) -> Schema {
        let mut columns = self.columns.clone();
        columns.push(col);
        Schema::new(columns)
    }
}

impl fmt::Display for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in self.columns.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{} {}", c.name, c.data_type.name())?;
        }
        Ok(())
    }
}

/// A full table definition as seen by the SQL layers.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDef {
    pub name: String,
    pub schema: Schema,
}

#[derive(Debug)]
pub enum DbError {
    Parse(String),
    Bind(String),
    Type(String),
    Constraint(String),
    NotFound(String),
    Storage(String),
    Conflict(String),
    Network(String),
    Internal(String),
    Io(std::io::Error),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Parse(m) => write!(f, "parse error: {}", m),
            DbError::Bind(m) => write!(f, "binding error: {}", m),
            DbError::Type(m) => write!(f, "type error: {}", m),
            DbError::Constraint(m) => write!(f, "constraint violation: {}", m),
            DbError::NotFound(m) => write!(f, "not found: {}", m),
            DbError::Storage(m) => write!(f, "storage error: {}", m),
            DbError::Conflict(m) => write!(f, "conflict: {}", m),
            DbError::Network(m) => write!(f, "network error: {}", m),
            DbError::Internal(m) => write!(f, "internal error: {}", m),
            DbError::Io(e) => write!(f, "io error: {}", e),
        }
    }
}

impl std::error::Error for DbError {}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, DbError>;

pub mod codec {
    //! Hand-rolled little-endian binary codec shared by storage and the wire
    //! protocol. Deliberately dependency-free.

    use super::{ColumnDef, DataType, DbError, Result, Schema, Value};

    pub fn put_u8(out: &mut Vec<u8>, v: u8) {
        out.push(v);
    }

    pub fn put_u32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_u64(out: &mut Vec<u8>, v: u64) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_i64(out: &mut Vec<u8>, v: i64) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_f64(out: &mut Vec<u8>, v: f64) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_bool(out: &mut Vec<u8>, v: bool) {
        out.push(if v { 1 } else { 0 });
    }

    pub fn put_bytes(out: &mut Vec<u8>, v: &[u8]) {
        put_u32(out, v.len() as u32);
        out.extend_from_slice(v);
    }

    pub fn put_str(out: &mut Vec<u8>, v: &str) {
        put_bytes(out, v.as_bytes());
    }

    pub struct Reader<'a> {
        buf: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        pub fn new(buf: &'a [u8]) -> Self {
            Reader { buf, pos: 0 }
        }

        pub fn remaining(&self) -> usize {
            self.buf.len() - self.pos
        }

        pub fn done(&self) -> bool {
            self.pos >= self.buf.len()
        }

        fn take(&mut self, n: usize) -> Result<&'a [u8]> {
            if self.remaining() < n {
                return Err(DbError::Storage("truncated encoded value".into()));
            }
            let s = &self.buf[self.pos..self.pos + n];
            self.pos += n;
            Ok(s)
        }

        pub fn u8(&mut self) -> Result<u8> {
            Ok(self.take(1)?[0])
        }

        pub fn u32(&mut self) -> Result<u32> {
            let b = self.take(4)?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        }

        pub fn u64(&mut self) -> Result<u64> {
            let b = self.take(8)?;
            Ok(u64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ]))
        }

        pub fn i64(&mut self) -> Result<i64> {
            Ok(self.u64()? as i64)
        }

        pub fn f64(&mut self) -> Result<f64> {
            Ok(f64::from_bits(self.u64()?))
        }

        pub fn bool(&mut self) -> Result<bool> {
            match self.u8()? {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(DbError::Storage("invalid bool byte".into())),
            }
        }

        pub fn bytes(&mut self) -> Result<&'a [u8]> {
            let len = self.u32()? as usize;
            self.take(len)
        }

        pub fn str(&mut self) -> Result<String> {
            let b = self.bytes()?;
            String::from_utf8(b.to_vec())
                .map_err(|_| DbError::Storage("invalid utf-8 in string".into()))
        }
    }

    const TAG_NULL: u8 = 0;
    const TAG_INT: u8 = 1;
    const TAG_FLOAT: u8 = 2;
    const TAG_BOOL: u8 = 3;
    const TAG_TEXT: u8 = 4;

    pub fn encode_value(v: &Value, out: &mut Vec<u8>) {
        match v {
            Value::Null => put_u8(out, TAG_NULL),
            Value::Int(i) => {
                put_u8(out, TAG_INT);
                put_i64(out, *i);
            }
            Value::Float(f) => {
                put_u8(out, TAG_FLOAT);
                put_f64(out, *f);
            }
            Value::Bool(b) => {
                put_u8(out, TAG_BOOL);
                put_bool(out, *b);
            }
            Value::Text(s) => {
                put_u8(out, TAG_TEXT);
                put_str(out, s);
            }
        }
    }

    pub fn decode_value(r: &mut Reader<'_>) -> Result<Value> {
        match r.u8()? {
            TAG_NULL => Ok(Value::Null),
            TAG_INT => Ok(Value::Int(r.i64()?)),
            TAG_FLOAT => Ok(Value::Float(r.f64()?)),
            TAG_BOOL => Ok(Value::Bool(r.bool()?)),
            TAG_TEXT => Ok(Value::Text(r.str()?)),
            t => Err(DbError::Storage(format!("unknown value tag {}", t))),
        }
    }

    pub fn encode_row(row: &[Value], out: &mut Vec<u8>) {
        put_u32(out, row.len() as u32);
        for v in row {
            encode_value(v, out);
        }
    }

    pub fn decode_row(r: &mut Reader<'_>) -> Result<Vec<Value>> {
        let n = r.u32()? as usize;
        let mut row = Vec::with_capacity(n);
        for _ in 0..n {
            row.push(decode_value(r)?);
        }
        Ok(row)
    }

    pub fn encode_schema(schema: &Schema, out: &mut Vec<u8>) {
        put_u32(out, schema.columns.len() as u32);
        for c in &schema.columns {
            put_str(out, &c.name);
            put_u8(out, match c.data_type {
                DataType::Int => 1,
                DataType::Float => 2,
                DataType::Bool => 3,
                DataType::Text => 4,
            });
            put_u8(out, (if c.nullable { 1 } else { 0 }) | (if c.primary_key { 2 } else { 0 }));
        }
    }

    pub fn decode_schema(r: &mut Reader<'_>) -> Result<Schema> {
        let n = r.u32()? as usize;
        let mut columns = Vec::with_capacity(n);
        for _ in 0..n {
            let name = r.str()?;
            let data_type = match r.u8()? {
                1 => DataType::Int,
                2 => DataType::Float,
                3 => DataType::Bool,
                4 => DataType::Text,
                t => return Err(DbError::Storage(format!("unknown type tag {}", t))),
            };
            let flags = r.u8()?;
            columns.push(ColumnDef {
                name,
                data_type,
                nullable: flags & 1 != 0,
                primary_key: flags & 2 != 0,
            });
        }
        Ok(Schema::new(columns))
    }
}
