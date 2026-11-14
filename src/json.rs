// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A minimal JSON parser, just enough to read cuttlefish_config.json.
//! Writing a parser by hand instead of using serde is the deliberate price of
//! the "zero dependencies" policy: a recursive descent over JSON takes a
//! hundred lines, and an offline build with the pinned toolchain is worth more
//! than a hundred lines.

use std::collections::HashMap;

// A general-purpose mini parser: not every value variant is read in every
// use, and that is fine for a small library like this.
#[allow(dead_code)]
#[derive(Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(HashMap<String, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(m) => m.get(key),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Num(n) => Some(*n as u64),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_obj(&self) -> Option<&HashMap<String, Value>> {
        match self {
            Value::Obj(m) => Some(m),
            _ => None,
        }
    }
}

pub fn parse(text: &str) -> Result<Value, String> {
    let bytes = text.as_bytes();
    let mut pos = 0;
    let v = value(bytes, &mut pos)?;
    Ok(v)
}

fn ws(b: &[u8], p: &mut usize) {
    while *p < b.len() && matches!(b[*p], b' ' | b'\t' | b'\n' | b'\r') {
        *p += 1;
    }
}

fn value(b: &[u8], p: &mut usize) -> Result<Value, String> {
    ws(b, p);
    match b.get(*p) {
        Some(b'{') => obj(b, p),
        Some(b'[') => arr(b, p),
        Some(b'"') => Ok(Value::Str(string(b, p)?)),
        Some(b't') => lit(b, p, "true", Value::Bool(true)),
        Some(b'f') => lit(b, p, "false", Value::Bool(false)),
        Some(b'n') => lit(b, p, "null", Value::Null),
        Some(_) => num(b, p),
        None => Err("unexpected end of json".into()),
    }
}

fn lit(b: &[u8], p: &mut usize, word: &str, v: Value) -> Result<Value, String> {
    if b[*p..].starts_with(word.as_bytes()) {
        *p += word.len();
        Ok(v)
    } else {
        Err(format!("bad literal at {p}"))
    }
}

fn num(b: &[u8], p: &mut usize) -> Result<Value, String> {
    let start = *p;
    while *p < b.len() && matches!(b[*p], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
        *p += 1;
    }
    std::str::from_utf8(&b[start..*p])
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Value::Num)
        .ok_or_else(|| format!("bad number at {start}"))
}

fn string(b: &[u8], p: &mut usize) -> Result<String, String> {
    *p += 1; // opening quote
    let mut out = String::new();
    while *p < b.len() {
        match b[*p] {
            b'"' => {
                *p += 1;
                return Ok(out);
            }
            b'\\' => {
                *p += 1;
                match b.get(*p) {
                    Some(b'n') => out.push('\n'),
                    Some(b't') => out.push('\t'),
                    Some(b'u') => {
                        // The cuttlefish config does not use \u, but the parser
                        // must not break: skip 4 hex digits.
                        *p += 4;
                        out.push('?');
                    }
                    Some(&c) => out.push(c as char),
                    None => return Err("truncated escape".into()),
                }
                *p += 1;
            }
            c => {
                out.push(c as char);
                *p += 1;
            }
        }
    }
    Err("unterminated string".into())
}

fn obj(b: &[u8], p: &mut usize) -> Result<Value, String> {
    *p += 1; // '{'
    let mut map = HashMap::new();
    ws(b, p);
    if b.get(*p) == Some(&b'}') {
        *p += 1;
        return Ok(Value::Obj(map));
    }
    loop {
        ws(b, p);
        let key = string(b, p)?;
        ws(b, p);
        if b.get(*p) != Some(&b':') {
            return Err(format!("expected ':' at {p}"));
        }
        *p += 1;
        map.insert(key, value(b, p)?);
        ws(b, p);
        match b.get(*p) {
            Some(b',') => *p += 1,
            Some(b'}') => {
                *p += 1;
                return Ok(Value::Obj(map));
            }
            _ => return Err(format!("expected ',' or '}}' at {p}")),
        }
    }
}

fn arr(b: &[u8], p: &mut usize) -> Result<Value, String> {
    *p += 1; // '['
    let mut items = Vec::new();
    ws(b, p);
    if b.get(*p) == Some(&b']') {
        *p += 1;
        return Ok(Value::Arr(items));
    }
    loop {
        items.push(value(b, p)?);
        ws(b, p);
        match b.get(*p) {
            Some(b',') => *p += 1,
            Some(b']') => {
                *p += 1;
                return Ok(Value::Arr(items));
            }
            _ => return Err(format!("expected ',' or ']' at {p}")),
        }
    }
}
