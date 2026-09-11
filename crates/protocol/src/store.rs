use anyhow::{ensure, Context};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Command};

pub fn path_info(paths: &[String]) -> anyhow::Result<Value> {
    let mut args = vec!["path-info", "--json", "--recursive"];
    args.extend(paths.iter().map(String::as_str));
    let output = Command::new("nix").args(args).output()?;
    ensure!(
        output.status.success(),
        "cannot read Nix closure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    normalize_closure(serde_json::from_slice(&output.stdout)?)
}

fn normalize_closure(value: Value) -> anyhow::Result<Value> {
    let entries = if let Some(map) = value.as_object() {
        map.iter()
            .map(|(path, info)| (path.clone(), info.clone()))
            .collect::<Vec<_>>()
    } else {
        value
            .as_array()
            .context("unexpected nix path-info format")?
            .iter()
            .map(|info| {
                Ok((
                    info["path"]
                        .as_str()
                        .context("closure entry missing path")?
                        .to_string(),
                    info.clone(),
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    };
    let mut normalized = serde_json::Map::new();
    for (path, info) in entries {
        let hash = info["narHash"]
            .as_str()
            .context("closure entry missing NAR hash")?;
        let mut references = info["references"]
            .as_array()
            .context("closure entry missing references")?
            .clone();
        references.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        // Registration times and signatures belong to the local store, not task identity.
        normalized.insert(
            path,
            json!({"narHash": hash, "narSize": info["narSize"], "references": references}),
        );
    }
    Ok(Value::Object(normalized))
}

pub fn closure_paths(value: &Value) -> anyhow::Result<Vec<PathBuf>> {
    Ok(value
        .as_object()
        .context("expected normalized Nix closure")?
        .keys()
        .map(PathBuf::from)
        .collect())
}

pub fn current_system() -> anyhow::Result<String> {
    let output = Command::new("nix")
        .args([
            "eval",
            "--raw",
            "--impure",
            "--expr",
            "builtins.currentSystem",
        ])
        .output()?;
    ensure!(output.status.success(), "cannot determine Nix system");
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closure_identity_excludes_store_local_state() {
        let left = json!({"/nix/store/example": {"narHash": "sha256-test", "narSize": 4, "references": ["b", "a"], "registrationTime": 10, "ultimate": true}});
        let right = json!([{ "path": "/nix/store/example", "narHash": "sha256-test", "narSize": 4, "references": ["a", "b"], "registrationTime": 50, "ultimate": false }]);
        assert_eq!(
            normalize_closure(left).unwrap(),
            normalize_closure(right).unwrap()
        );
    }
}
