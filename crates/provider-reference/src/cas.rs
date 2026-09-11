use crate::model::{InputArtifact, OutputArtifact};
use anyhow::{ensure, Context};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn materialize_input(
    input: &InputArtifact,
    destination: &Path,
    closure: &[PathBuf],
) -> anyhow::Result<()> {
    crate::model::store_path(&input.source)?;
    let source = Path::new(&input.source);
    validate_input_tree(source, closure)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    std::os::unix::fs::symlink(source, destination)?;
    Ok(())
}

fn resolve_input_path(source: &Path, closure: &[PathBuf]) -> anyhow::Result<PathBuf> {
    ensure!(source.is_absolute(), "input source must be absolute");
    let mut pending = source.to_path_buf();
    let mut links = 0;
    'resolve: loop {
        let mut resolved = PathBuf::new();
        let mut components = pending.components();
        while let Some(component) = components.next() {
            match component {
                Component::RootDir => resolved.push("/"),
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(name) => {
                    resolved.push(name);
                    let contained = closure.iter().any(|root| resolved.starts_with(root));
                    ensure!(
                        contained || closure.iter().any(|root| root.starts_with(&resolved)),
                        "input path is outside task closure: {}",
                        resolved.display()
                    );
                    let metadata = fs::symlink_metadata(&resolved)
                        .with_context(|| format!("resolving input {}", resolved.display()))?;
                    if metadata.file_type().is_symlink() {
                        ensure!(
                            contained,
                            "input link is outside task closure: {}",
                            resolved.display()
                        );
                        links += 1;
                        ensure!(links <= 40, "too many input symlink hops");
                        let link = fs::read_link(&resolved)?;
                        // Expand links before handling any following '..' component.
                        let mut target = resolved
                            .parent()
                            .context("input link has no parent")?
                            .join(link);
                        target.push(components.as_path());
                        pending = target;
                        continue 'resolve;
                    }
                    ensure!(
                        metadata.is_dir() || components.as_path().as_os_str().is_empty(),
                        "input traverses a non-directory: {}",
                        resolved.display()
                    );
                }
                Component::Prefix(_) => anyhow::bail!("unsupported input path prefix"),
            }
        }
        ensure!(
            closure.iter().any(|root| resolved.starts_with(root)),
            "input path is outside task closure: {}",
            resolved.display()
        );
        return Ok(resolved);
    }
}

fn validate_input_tree(source: &Path, closure: &[PathBuf]) -> anyhow::Result<()> {
    let mut pending = vec![source.to_path_buf()];
    let mut visited = BTreeSet::new();
    while let Some(path) = pending.pop() {
        let resolved = resolve_input_path(&path, closure)?;
        let metadata = fs::metadata(&path)?;
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "input contains a special file: {}",
            path.display()
        );
        // Canonical directory identities prevent cycles and repeated traversal of linked trees.
        if metadata.is_dir() && visited.insert(resolved.clone()) {
            for entry in fs::read_dir(resolved)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}

fn checked_output(root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    crate::model::relative_path(relative)?;
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        path.push(component);
        ensure!(
            !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "output symlinks are unsupported"
        );
    }
    ensure!(
        path.canonicalize()?.starts_with(root.canonicalize()?),
        "output escapes workspace"
    );
    Ok(path)
}

fn archive_directory(root: &Path, file: &mut fs::File) -> anyhow::Result<()> {
    fn append(
        builder: &mut tar::Builder<&mut fs::File>,
        root: &Path,
        relative: &Path,
    ) -> anyhow::Result<()> {
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "output contains a symlink or special file: {}",
            path.display()
        );
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        if metadata.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_cksum();
            builder.append_data(&mut header, relative, std::io::empty())?;
            let mut entries = fs::read_dir(&path)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                append(builder, root, &relative.join(entry.file_name()))?;
            }
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(metadata.len());
            header.set_cksum();
            builder.append_data(&mut header, relative, fs::File::open(&path)?)?;
        }
        Ok(())
    }
    let mut builder = tar::Builder::new(file);
    append(&mut builder, root, Path::new("."))?;
    builder.finish()?;
    Ok(())
}

pub fn collect_output(
    name: &str,
    output: &OutputArtifact,
    root: &Path,
    node_rank: u32,
) -> anyhow::Result<Value> {
    let path = checked_output(root, &output.path)?;
    ensure!(
        (output.kind == "file" && path.is_file()) || (output.kind == "directory" && path.is_dir()),
        "output type differs from declared {}",
        output.kind
    );
    let mut archive = if output.kind == "directory" {
        let mut file = tempfile::NamedTempFile::new()?;
        archive_directory(&path, file.as_file_mut())?;
        file.flush()?;
        Some(file)
    } else {
        None
    };
    let content = archive.as_mut().map(|file| file.path()).unwrap_or(&path);
    let digest = sha256_file(content)?;
    let destination = output.destination.as_ref().map(|destination| {
        format!(
            "{}/{}/{name}",
            destination.trim_end_matches('/'),
            if output.scope == "per-node" {
                format!("nodes/{node_rank}")
            } else {
                "leader".into()
            }
        )
    });
    let uri = destination
        .as_ref()
        .map(|destination| upload_output(content, destination, &digest))
        .transpose()?;
    Ok(
        json!({"name": name, "path": output.path, "kind": output.kind, "scope": output.scope, "node_rank": node_rank,
        "sha256": digest, "encoding": if output.kind == "directory" { "tar" } else { "identity" }, "uri": uri}),
    )
}

fn upload_output(path: &Path, destination: &str, digest: &str) -> anyhow::Result<String> {
    if let Some(root) = destination.strip_prefix("file://") {
        let target = PathBuf::from(root).join("sha256").join(digest);
        fs::create_dir_all(target.parent().context("missing output parent")?)?;
        let mut temporary = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
        std::io::copy(&mut fs::File::open(path)?, &mut temporary)?;
        temporary.persist(&target)?;
        Ok(format!("file://{}", target.display()))
    } else if let Some(rest) = destination.strip_prefix("s3://") {
        let endpoint = std::env::var("NIX_COMPUTE_S3_ENDPOINT").context(
            "s3:// outputs require NIX_COMPUTE_S3_ENDPOINT (provider-managed HTTP gateway)",
        )?;
        let url = format!("{}/{rest}/sha256/{digest}", endpoint.trim_end_matches('/'));
        ureq::put(&url).send(fs::File::open(path)?)?;
        Ok(url)
    } else {
        anyhow::bail!("unsupported output destination; use file:// or s3://")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_links_must_stay_in_the_immutable_closure() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let dependency = dir.path().join("dependency");
        fs::create_dir_all(input.join("nested")).unwrap();
        fs::create_dir(&dependency).unwrap();
        fs::write(dependency.join("data"), "immutable").unwrap();
        std::os::unix::fs::symlink(dependency.join("data"), input.join("nested/data")).unwrap();
        std::os::unix::fs::symlink("..", input.join("nested/parent")).unwrap();
        let closure = vec![input.clone(), dependency];
        validate_input_tree(&input, &closure).unwrap();
        let external = dir.path().join("external");
        fs::write(&external, "mutable").unwrap();
        std::os::unix::fs::symlink(external, input.join("nested/escape")).unwrap();
        assert!(validate_input_tree(&input, &closure)
            .unwrap_err()
            .to_string()
            .contains("outside task closure"));
        fs::remove_file(input.join("nested/escape")).unwrap();
        std::os::unix::fs::symlink("missing", input.join("dangling")).unwrap();
        assert!(validate_input_tree(&input, &closure).is_err());
    }

    #[test]
    fn input_rejects_external_file_and_directory_aliases_into_the_closure() {
        for directory in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let input = dir.path().join("input");
            let dependency = dir.path().join("dependency");
            fs::create_dir(&input).unwrap();
            fs::create_dir(&dependency).unwrap();
            fs::write(dependency.join("data"), "immutable").unwrap();
            let alias = dir.path().join("mutable-alias");
            let target = if directory {
                dependency.clone()
            } else {
                dependency.join("data")
            };
            std::os::unix::fs::symlink(target, &alias).unwrap();
            let target = if directory { alias.join("data") } else { alias };
            std::os::unix::fs::symlink(target, input.join("data")).unwrap();
            let closure = vec![input.clone(), dependency];
            assert!(input
                .join("data")
                .canonicalize()
                .unwrap()
                .starts_with(&closure[1]));
            assert!(validate_input_tree(&input, &closure)
                .unwrap_err()
                .to_string()
                .contains("outside task closure"));
        }
    }

    #[test]
    fn input_resolves_internal_links_before_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let dependency = dir.path().join("dependency");
        fs::create_dir(&input).unwrap();
        fs::create_dir_all(dependency.join("inner")).unwrap();
        fs::write(dependency.join("data"), "immutable").unwrap();
        std::os::unix::fs::symlink(dependency.join("inner"), input.join("indirect")).unwrap();
        std::os::unix::fs::symlink("indirect/../data", input.join("sample")).unwrap();
        let closure = vec![input.clone(), dependency.clone()];
        assert_eq!(
            resolve_input_path(&input.join("sample"), &closure).unwrap(),
            dependency.join("data")
        );
        validate_input_tree(&input, &closure).unwrap();
        std::os::unix::fs::symlink("loop", input.join("loop")).unwrap();
        assert!(validate_input_tree(&input, &closure).is_err());
    }

    fn output() -> OutputArtifact {
        OutputArtifact {
            path: "model".into(),
            kind: "directory".into(),
            scope: "per-node".into(),
            required: true,
            destination: None,
        }
    }

    #[test]
    fn directory_hash_is_stable_and_namespaces_each_node() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model");
        fs::create_dir_all(model.join("empty")).unwrap();
        fs::write(model.join("weights.bin"), "weights").unwrap();
        let mut spec = output();
        spec.destination = Some(format!("file://{}", dir.path().join("published").display()));
        let first = collect_output("checkpoint", &spec, dir.path(), 0).unwrap();
        let second = collect_output("checkpoint", &spec, dir.path(), 1).unwrap();
        assert_eq!(first["sha256"], second["sha256"]);
        assert_ne!(first["uri"], second["uri"]);
        let archive = fs::File::open(
            first["uri"]
                .as_str()
                .unwrap()
                .strip_prefix("file://")
                .unwrap(),
        )
        .unwrap();
        let names = tar::Archive::new(archive)
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert!(names.iter().any(|p| p.ends_with("weights.bin")));
        assert!(names.iter().any(|p| p.ends_with("empty")));
        fs::write(model.join("weights.bin"), "changed").unwrap();
        assert_ne!(
            first["sha256"],
            collect_output("checkpoint", &spec, dir.path(), 0).unwrap()["sha256"]
        );
    }

    #[test]
    fn output_rejects_symlinks_and_wrong_types() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("model")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("model/escape")).unwrap();
        assert!(collect_output("checkpoint", &output(), dir.path(), 0).is_err());
        let mut file = output();
        file.kind = "file".into();
        assert!(collect_output("checkpoint", &file, dir.path(), 0).is_err());
    }
}
