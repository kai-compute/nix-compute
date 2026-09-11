use anyhow::{ensure, Context};
use nix_compute_protocol::{
    model::{safe_name, ComputeJob, Target},
    store::path_info,
};
use serde_json::{json, Value};
use std::{fs, process::Command};

#[derive(Debug, Clone)]
pub struct Selector {
    pub flake: String,
    pub job: String,
}

impl Selector {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        let (flake, job) = value
            .rsplit_once('#')
            .context("selector must be <flake>#<job>")?;
        ensure!(
            !flake.is_empty() && !flake.starts_with('-') && safe_name(job),
            "invalid flake or job selector"
        );
        Ok(Self {
            flake: flake.into(),
            job: job.into(),
        })
    }

    // Evaluate and build the same immutable source snapshot throughout a run.
    pub fn snapshot(&self) -> anyhow::Result<(Self, Value)> {
        let metadata = command_json(&[
            "flake",
            "metadata",
            "--json",
            "--no-update-lock-file",
            &self.flake,
        ])?;
        let archived_ref = if metadata["revision"].is_string() {
            metadata["url"]
                .as_str()
                .context("locked Flake URL missing")?
        } else {
            &self.flake
        };
        let archive = command_json(&[
            "flake",
            "archive",
            "--json",
            "--no-update-lock-file",
            archived_ref,
        ])?;
        let root = archive["path"]
            .as_str()
            .context("Nix archive has no source path")?;
        let dir = metadata["locked"]["dir"].as_str().unwrap_or("");
        if !dir.is_empty() {
            crate::model::relative_path(dir)?;
        }
        let path = std::path::Path::new(root).join(dir);
        let lock: Value = serde_json::from_slice(
            &fs::read(path.join("flake.lock"))
                .context("submitted Flake must contain flake.lock")?,
        )?;
        let mut query = Vec::new();
        if !dir.is_empty() {
            query.push(format!("dir={dir}"));
        }
        if let Some(revision) = metadata["revision"].as_str() {
            ensure!(
                revision.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid source revision"
            );
            query.push(format!("rev={revision}"));
        }
        if let Some(timestamp) = metadata["lastModified"].as_u64() {
            query.push(format!("lastModified={timestamp}"));
        }
        let reference = format!(
            "path:{root}{}",
            if query.is_empty() {
                String::new()
            } else {
                format!("?{}", query.join("&"))
            }
        );
        let frozen = command_json(&[
            "flake",
            "metadata",
            "--json",
            "--no-update-lock-file",
            &reference,
        ])?;
        let source = path_info(&[root.to_string()])?;
        Ok((
            Self {
                flake: reference,
                job: self.job.clone(),
            },
            json!({"metadata": frozen, "lock": lock, "source": source, "archive": archive}),
        ))
    }

    pub fn job(&self) -> anyhow::Result<ComputeJob> {
        let projection = r#"j: if (j.schema_version or 1) != 3 then throw "unsupported schema; migrate inputs to source and rebuild with schema v3" else builtins.removeAttrs j [ "targets" ] // { targets = builtins.mapAttrs (_: t: { inherit (t) name system executor accelerator resources execution; }) j.targets; }"#;
        let apply = format!("jobs: if jobs ? \"{0}\" then ({projection}) jobs.\"{0}\" else if builtins.any (name: builtins.match \".*-(linux|darwin)\" name != null) (builtins.attrNames jobs) then throw \"schema v1 is unsupported; migrate to compute.jobs.<job>.targets and perTarget\" else throw \"unknown compute job {0}\"", self.job);
        let value = eval(&self.flake, "computeJobs", Some(&apply))?;
        let job: ComputeJob = serde_json::from_value(value)?;
        job.validate()?;
        Ok(job)
    }

    pub fn target(&self, name: &str) -> anyhow::Result<Target> {
        ensure!(safe_name(name), "invalid target name");
        let target: Target = serde_json::from_value(eval(
            &self.flake,
            &format!("computeJobs.\"{}\".targets.\"{name}\"", self.job),
            None,
        )?)?;
        target.validate()?;
        Ok(target)
    }

    pub fn build(&self, output: &str) -> anyhow::Result<String> {
        let paths = command_text(&[
            "build",
            "--no-link",
            "--print-out-paths",
            "--no-update-lock-file",
            &format!("{}#{output}", self.flake),
        ])?;
        let paths: Vec<_> = paths.lines().filter(|s| !s.is_empty()).collect();
        ensure!(
            paths.len() == 1 && paths[0].starts_with("/nix/store/"),
            "expected one Nix output path"
        );
        Ok(paths[0].into())
    }
}

fn eval(flake: &str, output: &str, apply: Option<&str>) -> anyhow::Result<Value> {
    let installable = format!("{flake}#{output}");
    let mut args = vec!["eval", "--json", "--no-update-lock-file", &installable];
    if let Some(apply) = apply {
        args.extend(["--apply", apply]);
    }
    command_json(&args)
}

fn command_json(args: &[&str]) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&command_text(args)?)?)
}

fn command_text(args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("nix").args(args).output()?;
    ensure!(
        output.status.success(),
        "nix failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selectors_cannot_inject_nix_attributes() {
        assert!(Selector::parse(".#train").is_ok());
        for selector in ["#train", "flake", ".#x.y", ".#\"evil\"", "--expr#train"] {
            assert!(Selector::parse(selector).is_err());
        }
    }
}
