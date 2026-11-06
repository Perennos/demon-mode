//! JSON in and out, matching the Python release's `json` usage.
//!
//! Output reproduces `json.dumps(..., ensure_ascii=True)`: sorted keys (the callers' maps are
//! already sorted), `\uXXXX` escapes for everything outside printable ASCII, Python float `repr`
//! (`18.0`, `1e-05`, `1e+16`), and either `indent=2` or compact separators. Input parsing can reject
//! duplicate object keys, like the Python `object_pairs_hook` that refused them, so a payload such as
//! `{"ordinaryUsageAllowed": false, "ordinaryUsageAllowed": true}` cannot be resolved by last-wins.

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::fmt;

/// `json.dumps(value, ensure_ascii=True, sort_keys=True, indent=2)`.
pub fn pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, Some(0));
    out
}

/// `json.dumps(value, ensure_ascii=True, separators=(",", ":"))`.
pub fn compact(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, None);
    out
}

/// A JSON string literal with Python's `ensure_ascii` escaping.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    write_string(&mut out, text);
    out
}

fn newline(out: &mut String, level: usize) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_value(out: &mut String, value: &Value, level: Option<usize>) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(out, number),
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if let Some(level) = level {
                    newline(out, level + 1);
                }
                write_value(out, item, level.map(|l| l + 1));
            }
            if let Some(level) = level {
                newline(out, level);
            }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if let Some(level) = level {
                    newline(out, level + 1);
                }
                write_string(out, key);
                out.push(':');
                if level.is_some() {
                    out.push(' ');
                }
                write_value(out, item, level.map(|l| l + 1));
            }
            if let Some(level) = level {
                newline(out, level);
            }
            out.push('}');
        }
    }
}

fn write_number(out: &mut String, number: &Number) {
    if let Some(value) = number.as_i64() {
        out.push_str(&value.to_string());
    } else if let Some(value) = number.as_u64() {
        out.push_str(&value.to_string());
    } else if let Some(value) = number.as_f64() {
        out.push_str(&float_repr(value));
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Python's `float.__repr__`: shortest round-trip digits, scientific below 1e-4 and from 1e16.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0.0");
    }
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    if !(-4..16).contains(&exponent) {
        let mantissa = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits };
        let mark = if exponent < 0 { '-' } else { '+' };
        return format!("{sign}{mantissa}e{mark}{:02}", exponent.abs());
    }
    if exponent >= 0 {
        let whole = exponent as usize + 1;
        if digits.len() > whole {
            format!("{sign}{}.{}", &digits[..whole], &digits[whole..])
        } else {
            format!("{sign}{digits}{}.0", "0".repeat(whole - digits.len()))
        }
    } else {
        format!("{sign}0.{}{digits}", "0".repeat((-exponent - 1) as usize))
    }
}

/// A float as a JSON value (finite policy numbers only).
pub fn float(value: f64) -> Value {
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

pub fn opt_float(value: Option<f64>) -> Value {
    value.map_or(Value::Null, float)
}

/// Parse JSON text, refusing duplicate object keys at any depth.
pub fn parse_strict(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str::<Strict>(text).map(|strict| strict.0)
}

struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("JSON")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value).map(Value::Number).ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut map = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            let Strict(item) = access.next_value()?;
            if map.insert(key, item).is_some() {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
        }
        Ok(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_match_python_repr() {
        let cases = [
            (18.0, "18.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (123_456_789_012_345_680.0, "1.2345678901234568e+17"),
            (0.6000000000000001, "0.6000000000000001"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (0.05, "0.05"),
            (-2.5, "-2.5"),
            (100.0, "100.0"),
            (0.0, "0.0"),
            (9_999_999_999_999_998.0, "9999999999999998.0"),
        ];
        for (value, expected) in cases {
            assert_eq!(float_repr(value), expected);
        }
    }

    #[test]
    fn layout_matches_python_json_dumps() {
        let value = json!({"b": [1, 2.0], "a": {"x": null}, "e": [], "o": {}, "s": "café — \u{1F600}\"\u{7f}"});
        assert_eq!(
            pretty(&value),
            "{\n  \"a\": {\n    \"x\": null\n  },\n  \"b\": [\n    1,\n    2.0\n  ],\n  \"e\": [],\n  \"o\": {},\n  \"s\": \"caf\\u00e9 \\u2014 \\ud83d\\ude00\\\"\\u007f\"\n}"
        );
        assert_eq!(compact(&json!({"k": [true, false]})), "{\"k\":[true,false]}");
    }

    #[test]
    fn strict_parse_rejects_duplicate_keys_only() {
        assert!(parse_strict(r#"{"a": 1, "a": 2}"#).is_err());
        assert!(parse_strict(r#"{"a": {"b": 1, "b": 1}}"#).is_err());
        assert_eq!(parse_strict(r#"{"a": [1, {"b": 2}]}"#).unwrap(), json!({"a": [1, {"b": 2}]}));
        assert!(parse_strict(&format!("{}0{}", "[".repeat(1500), "]".repeat(1500))).is_err());
    }
}
