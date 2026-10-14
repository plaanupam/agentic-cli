use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSafetyTier {
    /// Executes immediately without user intervention.
    AutonomousRead,
    /// Suspends the calling agent until the user approves, edits, or rejects it.
    HitlWrite,
}

#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    Approve,
    ApproveWithModifiedArgs(Value),
    Reject { reason: String },
}

pub fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                out.insert(key.clone(), canonical_json(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}
