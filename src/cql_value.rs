use crate::schema::ColumnType;
use anyhow::{Result, anyhow};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i32),
    BigInt(i64),
    SmallInt(i16),
    TinyInt(i8),
    Float(f32),
    Double(f64),
    Boolean(bool),
    Text(String),
    Blob(Vec<u8>),
    Timestamp(i64),
    Uuid([u8; 16]),
    Inet(std::net::IpAddr),
    Set(Vec<Value>),
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
}

impl Value {
    /// Zero-copy wire encoding directly into `out` (no intermediate Vec allocation).
    pub fn write_wire(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Value::Int(v) => {
                out.extend_from_slice(&4i32.to_be_bytes());
                out.extend_from_slice(&v.to_be_bytes());
            }
            Value::BigInt(v) => {
                out.extend_from_slice(&8i32.to_be_bytes());
                out.extend_from_slice(&v.to_be_bytes());
            }
            Value::SmallInt(v) => {
                out.extend_from_slice(&2i32.to_be_bytes());
                out.extend_from_slice(&v.to_be_bytes());
            }
            Value::TinyInt(v) => {
                out.extend_from_slice(&1i32.to_be_bytes());
                out.push(*v as u8);
            }
            Value::Float(v) => {
                out.extend_from_slice(&4i32.to_be_bytes());
                out.extend_from_slice(&v.to_be_bytes());
            }
            Value::Double(v) => {
                out.extend_from_slice(&8i32.to_be_bytes());
                out.extend_from_slice(&v.to_be_bytes());
            }
            Value::Boolean(v) => {
                out.extend_from_slice(&1i32.to_be_bytes());
                out.push(*v as u8);
            }
            Value::Text(v) => {
                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                out.extend_from_slice(v.as_bytes());
            }
            Value::Blob(v) => {
                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                out.extend_from_slice(v);
            }
            Value::Timestamp(v) => {
                out.extend_from_slice(&8i32.to_be_bytes());
                out.extend_from_slice(&v.to_be_bytes());
            }
            Value::Uuid(v) => {
                out.extend_from_slice(&16i32.to_be_bytes());
                out.extend_from_slice(v);
            }
            Value::Inet(v) => match v {
                std::net::IpAddr::V4(a) => {
                    out.extend_from_slice(&4i32.to_be_bytes());
                    out.extend_from_slice(&a.octets());
                }
                std::net::IpAddr::V6(a) => {
                    out.extend_from_slice(&16i32.to_be_bytes());
                    out.extend_from_slice(&a.octets());
                }
            },
            Value::Set(values) => {
                let mut payload = Vec::new();
                payload.extend_from_slice(&(values.len() as i32).to_be_bytes());
                for e in values {
                    e.write_wire(&mut payload);
                }
                out.extend_from_slice(&(payload.len() as i32).to_be_bytes());
                out.extend_from_slice(&payload);
            }
            Value::List(values) => {
                let mut payload = Vec::new();
                payload.extend_from_slice(&(values.len() as i32).to_be_bytes());
                for e in values {
                    e.write_wire(&mut payload);
                }
                out.extend_from_slice(&(payload.len() as i32).to_be_bytes());
                out.extend_from_slice(&payload);
            }
            Value::Map(entries) => {
                let mut payload = Vec::new();
                payload.extend_from_slice(&(entries.len() as i32).to_be_bytes());
                for (k, v) in entries {
                    k.write_wire(&mut payload);
                    v.write_wire(&mut payload);
                }
                out.extend_from_slice(&(payload.len() as i32).to_be_bytes());
                out.extend_from_slice(&payload);
            }
        }
    }

    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_wire(&mut out);
        out
    }

    pub fn raw_bytes(&self) -> Vec<u8> {
        match self {
            Value::Null => Vec::new(),
            Value::Int(v) => v.to_be_bytes().to_vec(),
            Value::BigInt(v) => v.to_be_bytes().to_vec(),
            Value::SmallInt(v) => v.to_be_bytes().to_vec(),
            Value::TinyInt(v) => vec![*v as u8],
            Value::Float(v) => v.to_be_bytes().to_vec(),
            Value::Double(v) => v.to_be_bytes().to_vec(),
            Value::Boolean(v) => vec![*v as u8],
            Value::Text(v) => v.as_bytes().to_vec(),
            Value::Blob(v) => v.clone(),
            Value::Timestamp(v) => v.to_be_bytes().to_vec(),
            Value::Uuid(v) => v.to_vec(),
            Value::Inet(v) => match v {
                std::net::IpAddr::V4(a) => a.octets().to_vec(),
                std::net::IpAddr::V6(a) => a.octets().to_vec(),
            },
            Value::Set(values) => {
                let mut out = Vec::new();
                out.extend_from_slice(&(values.len() as i32).to_be_bytes());
                for v in values {
                    out.extend(v.to_wire());
                }
                out
            }
            Value::List(values) => {
                let mut out = Vec::new();
                out.extend_from_slice(&(values.len() as i32).to_be_bytes());
                for v in values {
                    out.extend(v.to_wire());
                }
                out
            }
            Value::Map(entries) => {
                let mut out = Vec::new();
                out.extend_from_slice(&(entries.len() as i32).to_be_bytes());
                for (k, v) in entries {
                    out.extend(k.to_wire());
                    out.extend(v.to_wire());
                }
                out
            }
        }
    }

    pub fn from_raw_bytes(raw: &[u8], col_type: &ColumnType) -> Result<Value> {
        let mut wire = Vec::with_capacity(4 + raw.len());
        wire.extend_from_slice(&(raw.len() as i32).to_be_bytes());
        wire.extend_from_slice(raw);
        Value::from_wire(&wire, col_type)
    }

    pub fn from_wire(data: &[u8], col_type: &ColumnType) -> Result<Value> {
        if data.len() < 4 {
            return Err(anyhow!("value is shorter than its length prefix"));
        }
        let len = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if len < 0 {
            return Ok(Value::Null);
        }
        let len = len as usize;
        if data.len() != 4 + len {
            return Err(anyhow!("value length prefix does not match payload"));
        }
        let raw = &data[4..];
        let v = match col_type {
            ColumnType::Int => Value::Int(i32::from_be_bytes(raw.try_into()?)),
            ColumnType::BigInt => Value::BigInt(i64::from_be_bytes(raw.try_into()?)),
            ColumnType::SmallInt => Value::SmallInt(i16::from_be_bytes(raw.try_into()?)),
            ColumnType::TinyInt => Value::TinyInt(raw[0] as i8),
            ColumnType::Float => Value::Float(f32::from_be_bytes(raw.try_into()?)),
            ColumnType::Double => Value::Double(f64::from_be_bytes(raw.try_into()?)),
            ColumnType::Boolean => Value::Boolean(raw[0] != 0),
            ColumnType::Text => Value::Text(String::from_utf8(raw.to_vec())?),
            ColumnType::Blob => Value::Blob(raw.to_vec()),
            ColumnType::Timestamp => Value::Timestamp(i64::from_be_bytes(raw.try_into()?)),
            ColumnType::Uuid => Value::Uuid(raw.try_into()?),
            ColumnType::Inet => {
                let v = match raw.len() {
                    4 => std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                        raw[0], raw[1], raw[2], raw[3],
                    )),
                    16 => {
                        let mut octets = [0u8; 16];
                        octets.copy_from_slice(raw);
                        std::net::IpAddr::V6(std::net::Ipv6Addr::from(octets))
                    }
                    _ => return Err(anyhow!("invalid inet address length {}", raw.len())),
                };
                Value::Inet(v)
            }
            ColumnType::Set(elem) => {
                if raw.len() < 4 {
                    return Err(anyhow!("set value too short"));
                }
                let count = i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
                if count < 0 {
                    return Err(anyhow!("negative set element count"));
                }
                let mut values = Vec::new();
                let mut off = 4usize;
                for _ in 0..count {
                    if off + 4 > raw.len() {
                        return Err(anyhow!("set value truncated"));
                    }
                    let elen =
                        i32::from_be_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                    if elen < 0 || off + 4 + elen as usize > raw.len() {
                        return Err(anyhow!("set value truncated"));
                    }
                    let elen = elen as usize;
                    let v = Value::from_wire(&raw[off..off + 4 + elen], elem)?;
                    values.push(v);
                    off += 4 + elen;
                }
                Value::Set(values)
            }
            ColumnType::List(elem) => {
                if raw.len() < 4 {
                    return Err(anyhow!("list value too short"));
                }
                let count = i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
                if count < 0 {
                    return Err(anyhow!("negative list element count"));
                }
                let mut values = Vec::new();
                let mut off = 4usize;
                for _ in 0..count {
                    if off + 4 > raw.len() {
                        return Err(anyhow!("list value truncated"));
                    }
                    let elen =
                        i32::from_be_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                    if elen < 0 || off + 4 + elen as usize > raw.len() {
                        return Err(anyhow!("list value truncated"));
                    }
                    let elen = elen as usize;
                    let v = Value::from_wire(&raw[off..off + 4 + elen], elem)?;
                    values.push(v);
                    off += 4 + elen;
                }
                Value::List(values)
            }
            ColumnType::Map(k, v) => {
                if raw.len() < 4 {
                    return Err(anyhow!("map value too short"));
                }
                let count = i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
                if count < 0 {
                    return Err(anyhow!("negative map entry count"));
                }
                let mut entries = Vec::new();
                let mut off = 4usize;
                for _ in 0..count {
                    if off + 4 > raw.len() {
                        return Err(anyhow!("map value truncated"));
                    }
                    let klen =
                        i32::from_be_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                    if klen < 0 || off + 4 + klen as usize > raw.len() {
                        return Err(anyhow!("map value truncated"));
                    }
                    let klen = klen as usize;
                    let key = Value::from_wire(&raw[off..off + 4 + klen], k)?;
                    off += 4 + klen;
                    if off + 4 > raw.len() {
                        return Err(anyhow!("map value truncated"));
                    }
                    let vlen =
                        i32::from_be_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                    if vlen < 0 || off + 4 + vlen as usize > raw.len() {
                        return Err(anyhow!("map value truncated"));
                    }
                    let vlen = vlen as usize;
                    let val = Value::from_wire(&raw[off..off + 4 + vlen], v)?;
                    entries.push((key, val));
                    off += 4 + vlen;
                }
                Value::Map(entries)
            }
        };
        Ok(v)
    }

    pub fn parse_literal(text: &str, col_type: &ColumnType) -> Result<Value> {
        let t = text.trim();
        match col_type {
            ColumnType::Int => t
                .parse::<i32>()
                .map(Value::Int)
                .map_err(|_| anyhow!("cannot parse '{text}' as int")),
            ColumnType::BigInt => t
                .parse::<i64>()
                .map(Value::BigInt)
                .map_err(|_| anyhow!("cannot parse '{text}' as bigint")),
            ColumnType::SmallInt => t
                .parse::<i16>()
                .map(Value::SmallInt)
                .map_err(|_| anyhow!("cannot parse '{text}' as smallint")),
            ColumnType::TinyInt => t
                .parse::<i8>()
                .map(Value::TinyInt)
                .map_err(|_| anyhow!("cannot parse '{text}' as tinyint")),
            ColumnType::Float => t
                .parse::<f32>()
                .map(Value::Float)
                .map_err(|_| anyhow!("cannot parse '{text}' as float")),
            ColumnType::Double => t
                .parse::<f64>()
                .map(Value::Double)
                .map_err(|_| anyhow!("cannot parse '{text}' as double")),
            ColumnType::Boolean => match t.to_uppercase().as_str() {
                "TRUE" => Ok(Value::Boolean(true)),
                "FALSE" => Ok(Value::Boolean(false)),
                _ => Err(anyhow!("cannot parse '{text}' as boolean")),
            },
            ColumnType::Text => Ok(Value::Text(unescape_string(t))),
            ColumnType::Blob => {
                let hex_text = t
                    .strip_prefix("0x")
                    .or_else(|| t.strip_prefix("0X"))
                    .unwrap_or(t);
                let bytes =
                    hex::decode(hex_text).map_err(|_| anyhow!("cannot parse '{text}' as blob"))?;
                Ok(Value::Blob(bytes))
            }
            ColumnType::Timestamp => t
                .parse::<i64>()
                .map(Value::Timestamp)
                .map_err(|_| anyhow!("cannot parse '{text}' as timestamp")),
            ColumnType::Uuid => {
                let mut out = [0u8; 16];
                let hex_text = t.trim_start_matches('\'').trim_end_matches('\'');
                let cleaned: String = hex_text.chars().filter(|c| *c != '-').collect();
                if cleaned.len() != 32 {
                    return Err(anyhow!("cannot parse '{text}' as uuid"));
                }
                hex::decode_to_slice(&cleaned, &mut out)
                    .map_err(|_| anyhow!("cannot parse '{text}' as uuid"))?;
                Ok(Value::Uuid(out))
            }
            ColumnType::Inet => t
                .parse::<std::net::IpAddr>()
                .map(Value::Inet)
                .map_err(|_| anyhow!("cannot parse '{text}' as inet")),
            ColumnType::Set(_) | ColumnType::List(_) | ColumnType::Map(_, _) => {
                Err(anyhow!("collection literals are not supported"))
            }
        }
    }
}

pub fn unescape_string(text: &str) -> String {
    let t = text.trim();
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        t[1..t.len() - 1].replace("''", "'")
    } else {
        t.to_string()
    }
}
