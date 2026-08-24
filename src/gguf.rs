//! Minimal GGUF header/metadata reader — just enough to pull the fields
//! `pull.rs` needs for the manifest sidecar (§3): architecture, MoE
//! expert counts, trained context length. Stops before tensor data, never
//! reads the (multi-GB) weights themselves.
//!
//! Not in SPEC.md's module list; added because `pull.rs`'s "parsed from
//! GGUF metadata" step needs a real reader somewhere and no other
//! component owns this.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct GgufMetadata {
    pub architecture: String,
    pub context_length: Option<u64>,
    pub expert_count_total: Option<u64>,
    pub expert_count_active: Option<u64>,
}

// Bool/Float/Array data isn't extracted (only UInt/Int/String are, via
// as_u64/as_str) but all variants must exist to correctly parse past
// them in the sequential metadata stream.
#[allow(dead_code)]
#[derive(Debug, Clone)]
enum Value {
    UInt(u64),
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Array(Vec<Value>),
}

impl Value {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Value::UInt(v) => Some(*v),
            Value::Int(v) => Some(*v as u64),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
}

fn read_u8(r: &mut impl Read) -> std::io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}
fn read_u16(r: &mut impl Read) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_f32(r: &mut impl Read) -> std::io::Result<f32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(f32::from_le_bytes(b))
}
fn read_f64(r: &mut impl Read) -> std::io::Result<f64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(f64::from_le_bytes(b))
}
fn read_gguf_string(r: &mut impl Read) -> std::io::Result<String> {
    let len = read_u64(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn read_value(r: &mut impl Read, value_type: u32) -> std::io::Result<Value> {
    Ok(match value_type {
        0 => Value::UInt(read_u8(r)? as u64),           // UINT8
        1 => Value::Int(read_u8(r)? as i8 as i64),       // INT8
        2 => Value::UInt(read_u16(r)? as u64),           // UINT16
        3 => Value::Int(read_u16(r)? as i16 as i64),     // INT16
        4 => Value::UInt(read_u32(r)? as u64),           // UINT32
        5 => Value::Int(read_u32(r)? as i32 as i64),     // INT32
        6 => Value::Float(read_f32(r)? as f64),          // FLOAT32
        7 => Value::Bool(read_u8(r)? != 0),               // BOOL
        8 => Value::String(read_gguf_string(r)?),         // STRING
        9 => {
            // ARRAY: element type + length, then elements
            let elem_type = read_u32(r)?;
            let len = read_u64(r)?;
            let mut items = Vec::with_capacity(len.min(1024) as usize);
            for _ in 0..len {
                items.push(read_value(r, elem_type)?);
            }
            Value::Array(items)
        }
        10 => Value::UInt(read_u64(r)?),                  // UINT64
        11 => Value::Int(read_u64(r)? as i64),             // INT64
        12 => Value::Float(read_f64(r)?),                  // FLOAT64
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown GGUF value type tag {other}"),
            ))
        }
    })
}

pub fn read_metadata(path: &Path) -> anyhow::Result<GgufMetadata> {
    let file = File::open(path)?;
    let mut r = BufReader::new(file);

    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        anyhow::bail!("not a GGUF file: bad magic");
    }
    let version = read_u32(&mut r)?;

    let (tensor_count, kv_count) = if version == 1 {
        (read_u32(&mut r)? as u64, read_u32(&mut r)? as u64)
    } else {
        (read_u64(&mut r)?, read_u64(&mut r)?)
    };
    let _ = tensor_count; // not needed; we stop before tensor info/data

    let mut kv: HashMap<String, Value> = HashMap::with_capacity(kv_count as usize);
    for _ in 0..kv_count {
        let key = read_gguf_string(&mut r)?;
        let value_type = read_u32(&mut r)?;
        let value = read_value(&mut r, value_type)?;
        kv.insert(key, value);
    }

    let architecture = kv
        .get("general.architecture")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let context_length = kv
        .get(&format!("{architecture}.context_length"))
        .and_then(Value::as_u64);

    let expert_count_total = kv
        .get(&format!("{architecture}.expert_count"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    let expert_count_active = kv
        .get(&format!("{architecture}.expert_used_count"))
        .and_then(Value::as_u64);

    Ok(GgufMetadata {
        architecture,
        context_length,
        expert_count_total,
        expert_count_active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual check against a real downloaded GGUF, not run in CI (path
    /// is machine-local). `cargo test -- --ignored gguf_real_file`.
    #[test]
    #[ignore]
    fn gguf_real_file() {
        let path = std::path::Path::new(
            "/Users/elcharitas/Workspace/nodelm/models/olmoe-1b-7b-instruct-q4_k_m.gguf",
        );
        let meta = read_metadata(path).expect("should parse header");
        assert_eq!(meta.architecture, "olmoe");
        assert!(meta.expert_count_total.unwrap_or(0) >= 8);
        println!("{meta:?}");
    }
}
