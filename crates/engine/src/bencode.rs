//! Minimal bencode codec with raw-slice support for infohash.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// Decoded bencode value. Maps preserve sorted keys (bencode requires sorted encode).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub fn dict_get(&self, key: &str) -> Option<&Value> {
        self.as_dict()?.get(key.as_bytes())
    }

    pub fn dict_get_int(&self, key: &str) -> Option<i64> {
        self.dict_get(key)?.as_int()
    }

    pub fn dict_get_bytes(&self, key: &str) -> Option<&[u8]> {
        self.dict_get(key)?.as_bytes()
    }

    pub fn dict_get_str(&self, key: &str) -> Option<&str> {
        self.dict_get(key)?.as_str()
    }

    pub fn dict_get_list(&self, key: &str) -> Option<&[Value]> {
        self.dict_get(key)?.as_list()
    }

    pub fn dict_get_dict(&self, key: &str) -> Option<&BTreeMap<Vec<u8>, Value>> {
        self.dict_get(key)?.as_dict()
    }
}

/// Decode a full buffer into a value; returns value and bytes consumed.
pub fn decode(input: &[u8]) -> Result<(Value, usize)> {
    let mut i = 0;
    let v = decode_at(input, &mut i)?;
    Ok((v, i))
}

/// Decode the entire buffer (must consume all bytes, allowing trailing whitespace none).
pub fn decode_full(input: &[u8]) -> Result<Value> {
    let (v, n) = decode(input)?;
    if n != input.len() {
        return Err(Error::Bencode(format!(
            "trailing data after value ({n}/{} bytes)",
            input.len()
        )));
    }
    Ok(v)
}

/// Find the raw byte range of key `info` dict value inside a torrent root dict.
/// Returns (start, end) exclusive end of the bencoded info dict (for SHA-1).
pub fn find_raw_dict_value<'a>(input: &'a [u8], key: &[u8]) -> Result<&'a [u8]> {
    let mut i = 0;
    if input.is_empty() || input[0] != b'd' {
        return Err(Error::Bencode("root is not a dict".into()));
    }
    i += 1; // 'd'
    while i < input.len() && input[i] != b'e' {
        let k = decode_at(input, &mut i)?;
        let key_bytes = k
            .as_bytes()
            .ok_or_else(|| Error::Bencode("dict key not a string".into()))?;
        let val_start = i;
        let _val = decode_at(input, &mut i)?;
        let val_end = i;
        if key_bytes == key {
            return Ok(&input[val_start..val_end]);
        }
    }
    Err(Error::Bencode(format!(
        "key {} not found in dict",
        String::from_utf8_lossy(key)
    )))
}

fn decode_at(input: &[u8], i: &mut usize) -> Result<Value> {
    if *i >= input.len() {
        return Err(Error::Bencode("unexpected end of input".into()));
    }
    match input[*i] {
        b'i' => decode_int(input, i),
        b'l' => decode_list(input, i),
        b'd' => decode_dict(input, i),
        b'0'..=b'9' => decode_bytes(input, i),
        c => Err(Error::Bencode(format!(
            "invalid type marker {:?} at {i}",
            c as char
        ))),
    }
}

fn decode_int(input: &[u8], i: &mut usize) -> Result<Value> {
    *i += 1; // 'i'
    let start = *i;
    while *i < input.len() && input[*i] != b'e' {
        *i += 1;
    }
    if *i >= input.len() {
        return Err(Error::Bencode("unterminated integer".into()));
    }
    let s = std::str::from_utf8(&input[start..*i])
        .map_err(|_| Error::Bencode("integer not utf8".into()))?;
    let v: i64 = s
        .parse()
        .map_err(|_| Error::Bencode(format!("bad integer {s:?}")))?;
    *i += 1; // 'e'
    Ok(Value::Int(v))
}

fn decode_bytes(input: &[u8], i: &mut usize) -> Result<Value> {
    let start = *i;
    while *i < input.len() && input[*i] != b':' {
        *i += 1;
    }
    if *i >= input.len() {
        return Err(Error::Bencode("unterminated string length".into()));
    }
    let len_s = std::str::from_utf8(&input[start..*i])
        .map_err(|_| Error::Bencode("string length not utf8".into()))?;
    let len: usize = len_s
        .parse()
        .map_err(|_| Error::Bencode(format!("bad string length {len_s:?}")))?;
    *i += 1; // ':'
    if *i + len > input.len() {
        return Err(Error::Bencode("string extends past end".into()));
    }
    let bytes = input[*i..*i + len].to_vec();
    *i += len;
    Ok(Value::Bytes(bytes))
}

fn decode_list(input: &[u8], i: &mut usize) -> Result<Value> {
    *i += 1; // 'l'
    let mut list = Vec::new();
    while *i < input.len() && input[*i] != b'e' {
        list.push(decode_at(input, i)?);
    }
    if *i >= input.len() {
        return Err(Error::Bencode("unterminated list".into()));
    }
    *i += 1; // 'e'
    Ok(Value::List(list))
}

fn decode_dict(input: &[u8], i: &mut usize) -> Result<Value> {
    *i += 1; // 'd'
    let mut map = BTreeMap::new();
    while *i < input.len() && input[*i] != b'e' {
        let k = decode_at(input, i)?;
        let key = k
            .as_bytes()
            .ok_or_else(|| Error::Bencode("dict key not string".into()))?
            .to_vec();
        let v = decode_at(input, i)?;
        map.insert(key, v);
    }
    if *i >= input.len() {
        return Err(Error::Bencode("unterminated dict".into()));
    }
    *i += 1; // 'e'
    Ok(Value::Dict(map))
}

/// Encode a value to bencode bytes. Dict keys are written in sorted order.
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(n) => {
            out.push(b'i');
            out.extend_from_slice(n.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        Value::List(items) => {
            out.push(b'l');
            for item in items {
                encode_into(item, out);
            }
            out.push(b'e');
        }
        Value::Dict(map) => {
            out.push(b'd');
            for (k, v) in map {
                // keys already sorted in BTreeMap
                out.extend_from_slice(k.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(k);
                encode_into(v, out);
            }
            out.push(b'e');
        }
    }
}

/// Build a dict from string keys (UTF-8).
pub fn dict_from_str_keys(pairs: impl IntoIterator<Item = (String, Value)>) -> Value {
    let mut map = BTreeMap::new();
    for (k, v) in pairs {
        map.insert(k.into_bytes(), v);
    }
    Value::Dict(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_int_string_list_dict() {
        let (v, n) = decode(b"i42e").unwrap();
        assert_eq!(v, Value::Int(42));
        assert_eq!(n, 4);

        let (v, _) = decode(b"4:spam").unwrap();
        assert_eq!(v.as_str(), Some("spam"));

        let (v, _) = decode(b"li1ei2ee").unwrap();
        assert_eq!(v.as_list().unwrap().len(), 2);

        let (v, _) = decode(b"d3:foo3:bare").unwrap();
        assert_eq!(v.dict_get_str("foo"), Some("bar"));
    }

    #[test]
    fn raw_info_slice() {
        // d4:infod4:name3:fooe e  — simplified
        let raw = b"d4:infod4:name3:fooee";
        let info = find_raw_dict_value(raw, b"info").unwrap();
        assert_eq!(info, b"d4:name3:fooe");
    }

    #[test]
    fn encode_roundtrip() {
        let v = dict_from_str_keys([
            ("foo".into(), Value::Bytes(b"bar".to_vec())),
            ("n".into(), Value::Int(-3)),
            (
                "l".into(),
                Value::List(vec![Value::Int(1), Value::Bytes(b"x".to_vec())]),
            ),
        ]);
        let enc = encode(&v);
        let dec = decode_full(&enc).unwrap();
        assert_eq!(dec, v);
        // keys sorted: foo, l, n
        assert_eq!(&enc[..], b"d3:foo3:bar1:lli1e1:xe1:ni-3ee");
    }
}
