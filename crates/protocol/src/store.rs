use anyhow::Context;
use base64::{engine::general_purpose::STANDARD, Engine};
use nix_bindings_expr::eval_state::{gc_register_my_thread, EvalState};
use nix_bindings_store::store::Store;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn path_info(paths: &[String]) -> anyhow::Result<Value> {
    nix_bindings_expr::eval_state::init()?;
    let _gc = gc_register_my_thread()?;
    let mut store = Store::open(None, [])?;
    let roots = paths
        .iter()
        .map(|path| store.parse_store_path(path))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let refs = roots.iter().collect::<Vec<_>>();
    let closure = store.compute_fs_closure(&refs, false, false, false)?;
    let mut normalized = serde_json::Map::new();
    for path in closure {
        let path = store.real_path(&path)?;
        let (nar_hash, nar_size, references) = query_path_info(&path)?;
        normalized.insert(
            path,
            json!({"narHash": nar_hash, "narSize": nar_size, "references": references}),
        );
    }
    Ok(Value::Object(normalized))
}

fn query_path_info(path: &str) -> anyhow::Result<(String, u64, Vec<String>)> {
    let state_dir = std::env::var_os("NIX_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/nix/var/nix"));
    let database = state_dir.join("db/db.sqlite");
    let db = Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (hash, nar_size, id): (String, Option<u64>, i64) = db.query_row(
        "SELECT hash, narSize, id FROM ValidPaths WHERE path = ?1",
        params![path],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let hash = hash
        .strip_prefix("sha256:")
        .context("Nix store returned a non-sha256 NAR hash")?;
    let bytes = hex::decode(hash)?;
    let nar_hash = format!("sha256-{}", STANDARD.encode(bytes));
    let mut refs = db.prepare(
        "SELECT p.path FROM Refs r JOIN ValidPaths p ON p.id = r.reference WHERE r.referrer = ?1",
    )?;
    let mut references = refs
        .query_map(params![id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    references.sort();
    Ok((
        nar_hash,
        nar_size.context("Nix store path has no NAR size")?,
        references,
    ))
}

#[cfg(test)]
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
    nix_bindings_expr::eval_state::init()?;
    let _gc = gc_register_my_thread()?;
    let store = Store::open(None, [])?;
    let mut eval = EvalState::new(store, [])?;
    let value = eval.eval_from_string("builtins.currentSystem", "<nix-compute>")?;
    eval.require_string(&value)
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
