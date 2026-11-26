//! Tool definitions and arguments shared by the venue servers.

use serde_json::{json, Map, Value};

use crate::chain::{ToolError, DEFAULT_SLIPPAGE_BPS, MAX_SLIPPAGE_BPS};

pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub properties: Vec<(&'static str, Value)>,
    pub required: &'static [&'static str],
    pub build: bool,
}

impl Tool {
    pub fn json(&self) -> Value {
        let props: Map<String, Value> = self.properties.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        let mut annotations = json!({ "readOnlyHint": true, "openWorldHint": true });
        // Build tools only read the chain; they never sign or send.
        if self.build {
            annotations["idempotentHint"] = json!(false);
        }
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": { "type": "object", "properties": props, "required": self.required, "additionalProperties": false, "$schema": "http://json-schema.org/draft-07/schema#" },
            "annotations": annotations,
        })
    }
}

pub fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}
pub fn token_arg() -> Value {
    string("Token: \"BNB\", \"WBNB\", \"USDT\", \"USDC\" or a BEP-20 address")
}
pub fn amount_arg() -> Value {
    string("Amount of the input token in whole units, e.g. 0.25")
}
pub fn account_arg() -> Value {
    string("The wallet that sends the transactions and receives the output")
}
pub fn slippage_arg() -> Value {
    json!({ "type": "integer", "minimum": 0, "maximum": MAX_SLIPPAGE_BPS, "description": format!("Max slippage in basis points (default {DEFAULT_SLIPPAGE_BPS} = 1%). Tax tokens need at least their tax on top.") })
}
pub fn deadline_arg() -> Value {
    json!({ "type": "integer", "minimum": 60, "maximum": 3600, "description": "Seconds until the swap expires (default 600)" })
}
pub fn enum_arg(values: &[&str], description: Option<&str>) -> Value {
    let mut v = json!({ "type": "string", "enum": values });
    if let Some(d) = description {
        v["description"] = json!(d);
    }
    v
}

/// A tool's arguments, checked against its schema (as the MCP SDK does before calling it).
pub struct Args<'a> {
    pub tool: &'a Tool,
    pub v: &'a Value,
}

/// Arguments that don't match the schema: reported as a tool error, as the SDK reports them.
pub struct Invalid(pub String);

impl<'a> Args<'a> {
    fn schema(&self, key: &str) -> &Value {
        &self.tool.properties.iter().find(|(k, _)| *k == key).expect("declared property").1
    }

    /// Every declared argument has the right type, range and enum value; required ones are present.
    pub fn check(&self) -> Result<(), Invalid> {
        for (key, schema) in &self.tool.properties {
            let v = &self.v[*key];
            if v.is_null() {
                if self.tool.required.contains(key) {
                    return Err(Invalid(format!("{key}: Required")));
                }
                continue;
            }
            match schema["type"].as_str() {
                Some("string") => {
                    let Some(s) = v.as_str() else { return Err(Invalid(format!("{key}: Expected string"))) };
                    if let Some(values) = schema["enum"].as_array() {
                        if !values.iter().any(|x| x == s) {
                            return Err(Invalid(format!("{key}: Invalid enum value")));
                        }
                    }
                }
                Some("integer") => {
                    let ok = v.as_f64().is_some_and(|f| f.fract() == 0.0);
                    let n = v.as_f64().unwrap_or(f64::NAN);
                    let (min, max) = (schema["minimum"].as_f64().unwrap_or(f64::MIN), schema["maximum"].as_f64().unwrap_or(f64::MAX));
                    if !ok || n < min || n > max {
                        return Err(Invalid(format!("{key}: Expected an integer from {min} to {max}")));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn str(&self, key: &str) -> &'a str {
        let _ = self.schema(key);
        self.v[key].as_str().unwrap_or("")
    }
    pub fn opt_str(&self, key: &str) -> Option<&'a str> {
        self.v[key].as_str()
    }
    pub fn opt_int(&self, key: &str) -> Option<i64> {
        self.v[key].as_f64().map(|f| f as i64)
    }
}

/// A tool result: pretty JSON text, or an error result with the message.
pub fn ok(data: &Value) -> Value {
    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(data).unwrap() }] })
}

pub fn fail(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

pub fn result(r: Result<Value, ToolError>) -> Value {
    match r {
        Ok(v) => ok(&v),
        Err(e) => fail(&e.0),
    }
}
