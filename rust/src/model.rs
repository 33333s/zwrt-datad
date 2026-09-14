use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DatadVersion {
    pub name: &'static str,
    pub version: &'static str,
}

impl Default for DatadVersion {
    fn default() -> Self {
        Self {
            name: "zwrt-datad",
            version: env!("CARGO_PKG_VERSION"),
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
