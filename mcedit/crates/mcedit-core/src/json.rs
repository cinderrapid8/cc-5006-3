//! Minimal, dependency-free JSON support used across the workspace.
//!
//! Only the subset needed by project files and the HTTP API is implemented:
//! objects preserve insertion order, numbers round-trip as f64.

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj() -> Self {
        Json::Obj(Vec::new())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64().map(|n| n as u64)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Vec<(String, Json)>> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn insert(&mut self, key: &str, value: Json) {
        if let Json::Obj(entries) = self {
            if let Some(slot) = entries.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                entries.push((key.to_string(), value));
            }
        }
    }

    pub fn from_str(input: &str) -> Result<Json, String> {
        let bytes = input.as_bytes();
        let mut pos = 0;
        let value = parse_value(bytes, &mut pos)?;
        skip_ws(bytes, &mut pos);
        if pos != bytes.len() {
            return Err(format!("trailing data at byte {}", pos));
        }
        Ok(value)
    }

    pub fn to_string_pretty(&self) -> String {
        let mut out = String::new();
        write_pretty(self, 0, &mut out);
        out.push('\n');
        out
    }

    pub fn to_compact_string(&self) -> String {
        let mut out = String::new();
        write_compact(self, &mut out);
        out
    }

    /// Canonical form used for content hashing: keys sorted, compact spacing.
    pub fn to_canonical_string(&self) -> String {
        let mut out = String::new();
        write_canonical(self, &mut out);
        out
    }
}

fn skip_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && matches!(b[*pos], b' ' | b'\t' | b'\n' | b'\r') {
        *pos += 1;
    }
}

fn parse_value(b: &[u8], pos: &mut usize) -> Result<Json, String> {
    skip_ws(b, pos);
    if *pos >= b.len() {
        return Err("unexpected end of input".to_string());
    }
    match b[*pos] {
        b'{' => parse_object(b, pos),
        b'[' => parse_array(b, pos),
        b'"' => Ok(Json::Str(parse_string(b, pos)?)),
        b't' | b'f' => parse_bool(b, pos),
        b'n' => parse_null(b, pos),
        b'-' | b'0'..=b'9' => parse_number(b, pos),
        other => Err(format!("unexpected byte {} at {}", other as char, pos)),
    }
}

fn parse_object(b: &[u8], pos: &mut usize) -> Result<Json, String> {
    *pos += 1;
    let mut entries = Vec::new();
    skip_ws(b, pos);
    if *pos < b.len() && b[*pos] == b'}' {
        *pos += 1;
        return Ok(Json::Obj(entries));
    }
    loop {
        skip_ws(b, pos);
        let key = parse_string(b, pos)?;
        skip_ws(b, pos);
        if *pos >= b.len() || b[*pos] != b':' {
            return Err(format!("expected ':' at {}", pos));
        }
        *pos += 1;
        let value = parse_value(b, pos)?;
        entries.push((key, value));
        skip_ws(b, pos);
        if *pos >= b.len() {
            return Err("unterminated object".to_string());
        }
        match b[*pos] {
            b',' => {
                *pos += 1;
            }
            b'}' => {
                *pos += 1;
                break;
            }
            other => return Err(format!("expected ',' or '}}' got {} at {}", other as char, pos)),
        }
    }
    Ok(Json::Obj(entries))
}

fn parse_array(b: &[u8], pos: &mut usize) -> Result<Json, String> {
    *pos += 1;
    let mut items = Vec::new();
    skip_ws(b, pos);
    if *pos < b.len() && b[*pos] == b']' {
        *pos += 1;
        return Ok(Json::Arr(items));
    }
    loop {
        let value = parse_value(b, pos)?;
        items.push(value);
        skip_ws(b, pos);
        if *pos >= b.len() {
            return Err("unterminated array".to_string());
        }
        match b[*pos] {
            b',' => {
                *pos += 1;
            }
            b']' => {
                *pos += 1;
                break;
            }
            other => return Err(format!("expected ',' or ']' got {} at {}", other as char, pos)),
        }
    }
    Ok(Json::Arr(items))
}

fn parse_string(b: &[u8], pos: &mut usize) -> Result<String, String> {
    if b[*pos] != b'"' {
        return Err(format!("expected string at {}", pos));
    }
    *pos += 1;
    let mut s = String::new();
    while *pos < b.len() {
        let c = b[*pos];
        *pos += 1;
        match c {
            b'"' => return Ok(s),
            b'\\' => {
                if *pos >= b.len() {
                    return Err("bad escape".to_string());
                }
                let e = b[*pos];
                *pos += 1;
                match e {
                    b'"' => s.push('"'),
                    b'\\' => s.push('\\'),
                    b'/' => s.push('/'),
                    b'b' => s.push('\u{0008}'),
                    b'f' => s.push('\u{000C}'),
                    b'n' => s.push('\n'),
                    b'r' => s.push('\r'),
                    b't' => s.push('\t'),
                    b'u' => {
                        if *pos + 4 > b.len() {
                            return Err("bad unicode escape".to_string());
                        }
                        let hex = std::str::from_utf8(&b[*pos..*pos + 4]).map_err(|e| e.to_string())?;
                        let code = u32::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
                        *pos += 4;
                        if let Some(ch) = char::from_u32(code) {
                            s.push(ch);
                        }
                    }
                    other => return Err(format!("bad escape \\{}", other as char)),
                }
            }
            _ => {
                // Collect a run of UTF-8 bytes at once.
                if c < 0x80 {
                    s.push(c as char);
                } else {
                    let start = *pos - 1;
                    let len = if c >= 0xF0 {
                        4
                    } else if c >= 0xE0 {
                        3
                    } else {
                        2
                    };
                    *pos = start + len;
                    let chunk = std::str::from_utf8(&b[start..*pos]).map_err(|e| e.to_string())?;
                    s.push_str(chunk);
                }
            }
        }
    }
    Err("unterminated string".to_string())
}

fn parse_bool(b: &[u8], pos: &mut usize) -> Result<Json, String> {
    if b[*pos..].starts_with(b"true") {
        *pos += 4;
        Ok(Json::Bool(true))
    } else if b[*pos..].starts_with(b"false") {
        *pos += 5;
        Ok(Json::Bool(false))
    } else {
        Err(format!("invalid literal at {}", pos))
    }
}

fn parse_null(b: &[u8], pos: &mut usize) -> Result<Json, String> {
    if b[*pos..].starts_with(b"null") {
        *pos += 4;
        Ok(Json::Null)
    } else {
        Err(format!("invalid literal at {}", pos))
    }
}

fn parse_number(b: &[u8], pos: &mut usize) -> Result<Json, String> {
    let start = *pos;
    if b[*pos] == b'-' {
        *pos += 1;
    }
    while *pos < b.len() && matches!(b[*pos], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
        *pos += 1;
    }
    let text = std::str::from_utf8(&b[start..*pos]).map_err(|e| e.to_string())?;
    text.parse::<f64>().map(Json::Num).map_err(|e| e.to_string())
}

fn escape_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn format_number(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        (n as i64).to_string()
    } else {
        let s = format!("{:.6}", n);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn write_compact(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => out.push_str(&format_number(*n)),
        Json::Str(s) => escape_string(s, out),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(item, out);
            }
            out.push(']');
        }
        Json::Obj(entries) => {
            out.push('{');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                escape_string(k, out);
                out.push(':');
                write_compact(val, out);
            }
            out.push('}');
        }
    }
}

fn write_pretty(v: &Json, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    let pad_inner = "  ".repeat(indent + 1);
    match v {
        Json::Arr(items) if items.is_empty() => out.push_str("[]"),
        Json::Obj(entries) if entries.is_empty() => out.push_str("{}"),
        Json::Arr(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad_inner);
                write_pretty(item, indent + 1, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push(']');
        }
        Json::Obj(entries) => {
            out.push_str("{\n");
            for (i, (k, val)) in entries.iter().enumerate() {
                out.push_str(&pad_inner);
                escape_string(k, out);
                out.push_str(": ");
                write_pretty(val, indent + 1, out);
                if i + 1 < entries.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push('}');
        }
        other => write_compact(other, out),
    }
}

fn write_canonical(v: &Json, out: &mut String) {
    match v {
        Json::Obj(entries) => {
            let sorted: BTreeMap<&str, &Json> = entries.iter().map(|(k, v)| (k.as_str(), v)).collect();
            out.push('{');
            for (i, (k, val)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                escape_string(k, out);
                out.push(':');
                write_canonical(val, out);
            }
            out.push('}');
        }
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => write_compact(other, out),
    }
}

/// Convenience constructors used by the rest of the crate.
pub fn str_val(s: impl Into<String>) -> Json {
    Json::Str(s.into())
}

pub fn num_val(n: f64) -> Json {
    Json::Num(n)
}
