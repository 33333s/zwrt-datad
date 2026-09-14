use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fs};

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DatadVersion {
    pub name: &'static str,
    pub version: &'static str,
}

impl Default for DatadVersion {
    fn default() -> Self {
        Self {
            name: "zwrt-datad",
            version: env!("DATAD_VERSION"),
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Snapshot {
    pub ts: i64,
    pub datad: DatadVersion,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub struct UbusCall {
    pub service: String,
    pub method: String,
    #[serde(default = "empty_object")]
    pub args: Value,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn shape(value: &Value, path: &str, out: &mut BTreeMap<String, &'static str>) {
    match value {
        Value::Object(values) => {
            for (key, value) in values {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                shape(value, &child, out);
            }
        }
        Value::Array(_) => {
            out.insert(path.into(), "array");
        }
        Value::Null => {
            out.insert(path.into(), "null");
        }
        Value::Bool(_) => {
            out.insert(path.into(), "boolean");
        }
        Value::Number(_) => {
            out.insert(path.into(), "number");
        }
        Value::String(_) => {
            out.insert(path.into(), "string");
        }
    }
}

/// Compare JSON structure without emitting values. This is intentionally a CLI
/// diagnostic so device-local snapshots never need to leave the router.
pub fn compare_state_shape(paths: &[String]) -> Result<Value, String> {
    if paths.len() != 2 {
        return Err("usage: --compare-state-shape BASELINE CANDIDATE".into());
    }
    let mut shapes = Vec::new();
    for path in paths {
        let metadata = fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
        if metadata.len() > 32 * 1024 * 1024 {
            return Err(format!("{path}: input exceeds 32 MiB"));
        }
        let data = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let value: Value = serde_json::from_slice(&data).map_err(|e| format!("{path}: {e}"))?;
        let mut current = BTreeMap::new();
        shape(&value, "", &mut current);
        shapes.push(current);
    }
    let baseline = &shapes[0];
    let candidate = &shapes[1];
    let missing: Vec<_> = baseline
        .keys()
        .filter(|key| !candidate.contains_key(*key))
        .cloned()
        .collect();
    let extra: Vec<_> = candidate
        .keys()
        .filter(|key| !baseline.contains_key(*key))
        .cloned()
        .collect();
    let type_mismatches: Vec<_> = baseline
        .iter()
        .filter_map(|(key, expected)| {
            let actual = candidate.get(key)?;
            (actual != expected).then(|| {
                serde_json::json!({
                    "path":key, "baseline":expected, "candidate":actual
                })
            })
        })
        .collect();
    let mut different_values = Vec::new();
    fn compare_values(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
        match (a, b) {
            (Value::Object(left), Value::Object(right)) => {
                for (key, value) in left {
                    if let Some(other) = right.get(key) {
                        let child = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        compare_values(value, other, &child, out);
                    }
                }
            }
            _ if a != b => out.push(path.into()),
            _ => {}
        }
    }
    let baseline_json: Value =
        serde_json::from_slice(&fs::read(&paths[0]).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let candidate_json: Value =
        serde_json::from_slice(&fs::read(&paths[1]).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    compare_values(&baseline_json, &candidate_json, "", &mut different_values);
    Ok(serde_json::json!({
        "baseline_paths":baseline.len(), "candidate_paths":candidate.len(),
        "missing":missing, "extra":extra, "type_mismatches":type_mismatches,
        "different_values":different_values
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_never_contains_values() {
        let mut out = BTreeMap::new();
        shape(
            &serde_json::json!({"secret":"12345","items":[{"id":7}]}),
            "",
            &mut out,
        );
        assert_eq!(out.get("secret"), Some(&"string"));
        assert_eq!(out.get("items"), Some(&"array"));
        assert!(!format!("{out:?}").contains("12345"));
    }
}
