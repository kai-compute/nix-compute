use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn canonical_json(value: &Value) -> anyhow::Result<Vec<u8>> {
    let mut output = String::new();
    write_value(value, &mut output)?;
    Ok(output.into_bytes())
}

fn write_value(value: &Value, output: &mut String) -> anyhow::Result<()> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            output.push_str(&serde_json::to_string(value)?);
        }
        Value::Array(values) => {
            output.push('[');
            for (index, item) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_value(item, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key)?);
                output.push(':');
                write_value(&values[*key], output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn sha256_value(value: &Value) -> anyhow::Result<String> {
    Ok(sha256_bytes(&canonical_json(value)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_order_does_not_change_digest() {
        let left: Value = serde_json::json!({"b": 2, "a": 1});
        let right: Value = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(sha256_value(&left).unwrap(), sha256_value(&right).unwrap());
    }
}
