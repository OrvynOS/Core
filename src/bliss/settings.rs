//! Declarative plugin settings. One spec drives the settings form in the UI and the server-side
//! validation, so what a creator can enter is exactly what a plugin accepts.

use serde::Serialize;
use serde_json::{Map, Value};

pub type Settings = Map<String, Value>;

#[derive(Debug, Clone, Serialize)]
pub struct SelectOption {
    pub value: &'static str,
    pub label: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SettingField {
    Number {
        key: &'static str,
        label: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        help: Option<&'static str>,
        default: f64,
        min: f64,
        max: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        step: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<&'static str>,
    },
    Boolean {
        key: &'static str,
        label: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        help: Option<&'static str>,
        default: bool,
    },
    Text {
        key: &'static str,
        label: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        help: Option<&'static str>,
        default: &'static str,
        #[serde(rename = "maxLength")]
        max_length: usize,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        multiline: bool,
    },
    List {
        key: &'static str,
        label: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        help: Option<&'static str>,
        default: Vec<&'static str>,
        #[serde(rename = "maxItems")]
        max_items: usize,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        uppercase: bool,
    },
    Select {
        key: &'static str,
        label: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        help: Option<&'static str>,
        default: &'static str,
        options: Vec<SelectOption>,
    },
}

impl SettingField {
    pub fn key(&self) -> &'static str {
        match self {
            Self::Number { key, .. } | Self::Boolean { key, .. } | Self::Text { key, .. } | Self::List { key, .. } | Self::Select { key, .. } => key,
        }
    }
    pub fn default_value(&self) -> Value {
        match self {
            Self::Number { default, .. } => num(*default),
            Self::Boolean { default, .. } => Value::Bool(*default),
            Self::Text { default, .. } => Value::String(default.to_string()),
            Self::List { default, .. } => Value::Array(default.iter().map(|s| Value::String(s.to_string())).collect()),
            Self::Select { default, .. } => Value::String(default.to_string()),
        }
    }

    /// Validate one value (as zod did: numbers coerced from strings, lists trimmed and de-duplicated).
    fn parse(&self, v: &Value) -> std::result::Result<Value, String> {
        match self {
            Self::Number { min, max, step, .. } => {
                let n = match v {
                    Value::Number(n) => n.as_f64(),
                    Value::String(s) if s.trim().is_empty() => Some(0.0),
                    Value::String(s) => s.trim().parse::<f64>().ok(),
                    Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
                    Value::Null => Some(0.0),
                    _ => None,
                }
                .filter(|n| n.is_finite())
                .ok_or("not a number")?;
                if n < *min || n > *max {
                    return Err("out of range".into());
                }
                if step.is_some_and(|s| s.fract() == 0.0) && n.fract() != 0.0 {
                    return Err("must be a whole number".into());
                }
                Ok(num(n))
            }
            Self::Boolean { .. } => v.as_bool().map(Value::Bool).ok_or_else(|| "not a boolean".into()),
            Self::Text { max_length, .. } => {
                let s = v.as_str().ok_or("not text")?;
                if s.encode_utf16().count() > *max_length {
                    return Err("too long".into());
                }
                Ok(Value::String(s.to_string()))
            }
            Self::List { max_items, uppercase, .. } => {
                let items = v.as_array().ok_or("not a list")?;
                if items.len() > *max_items {
                    return Err("too many items".into());
                }
                let mut out: Vec<String> = vec![];
                for item in items {
                    let s = item.as_str().ok_or("not text")?.trim().to_string();
                    if s.is_empty() || s.chars().count() > 64 {
                        return Err("bad item".into());
                    }
                    let s = if *uppercase { s.to_uppercase() } else { s };
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
                Ok(Value::Array(out.into_iter().map(Value::String).collect()))
            }
            Self::Select { options, .. } => {
                let s = v.as_str().ok_or("not text")?;
                if options.iter().any(|o| o.value == s) { Ok(Value::String(s.into())) } else { Err("not an option".into()) }
            }
        }
    }
}

/// A number as JSON, integral ones as integers (JavaScript has one number type).
pub fn num(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 { Value::from(n as i64) } else { Value::from(n) }
}

pub fn defaults_for(fields: &[SettingField]) -> Settings {
    fields.iter().map(|f| (f.key().to_string(), f.default_value())).collect()
}

/// Validate settings; missing keys fall back to defaults, unknown keys are dropped.
pub fn parse_settings(fields: &[SettingField], input: &Value) -> std::result::Result<Settings, String> {
    let empty = Map::new();
    let given = match input {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err("settings must be an object".into()),
    };
    let mut out = Map::new();
    for f in fields {
        let v = match given.get(f.key()) {
            None | Some(Value::Null) => f.default_value(),
            Some(v) => f.parse(v).map_err(|e| format!("{}: {e}", f.key()))?,
        };
        out.insert(f.key().to_string(), v);
    }
    Ok(out)
}

// Short constructors for plugin definitions.
pub fn number(key: &'static str, label: &'static str, help: Option<&'static str>, default: f64, min: f64, max: f64, step: Option<f64>, unit: Option<&'static str>) -> SettingField {
    SettingField::Number { key, label, help, default, min, max, step, unit }
}
pub fn boolean(key: &'static str, label: &'static str, help: Option<&'static str>, default: bool) -> SettingField {
    SettingField::Boolean { key, label, help, default }
}
pub fn text(key: &'static str, label: &'static str, help: Option<&'static str>, default: &'static str, max_length: usize, multiline: bool) -> SettingField {
    SettingField::Text { key, label, help, default, max_length, multiline }
}
pub fn list(key: &'static str, label: &'static str, help: Option<&'static str>, default: Vec<&'static str>, max_items: usize, uppercase: bool) -> SettingField {
    SettingField::List { key, label, help, default, max_items, uppercase }
}
pub fn select(key: &'static str, label: &'static str, help: Option<&'static str>, default: &'static str, options: &[(&'static str, &'static str)]) -> SettingField {
    SettingField::Select { key, label, help, default, options: options.iter().map(|(value, label)| SelectOption { value, label }).collect() }
}
