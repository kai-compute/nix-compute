use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Component, Path},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeJob {
    pub schema_version: u32,
    pub name: String,
    pub artifacts: Artifacts,
    pub parameters: BTreeMap<String, Value>,
    pub reproducibility: Reproducibility,
    pub targets: BTreeMap<String, TargetSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetSummary {
    pub name: String,
    pub system: String,
    pub executor: String,
    pub accelerator: Accelerator,
    pub resources: Resources,
    pub execution: Execution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    #[serde(flatten)]
    pub summary: TargetSummary,
    pub program_output: String,
    pub program: String,
    pub entrypoint: Vec<String>,
    pub probe_output: Option<String>,
    pub probe: Option<String>,
    pub image_output: Option<String>,
    pub image_name: String,
    pub runtime_paths: Vec<String>,
    pub runtime_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accelerator {
    pub id: String,
    pub count: u32,
    pub min_memory_mib: u64,
    pub architectures: Vec<String>,
    pub driver_range: Option<String>,
    pub runtime_version: Option<String>,
    pub topology: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub nodes: u32,
    pub cpu_cores: Option<u32>,
    pub memory_mib: Option<u64>,
    pub enforce: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub network: String,
    pub isolation: String,
    pub timeout_seconds: Option<u64>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifacts {
    pub inputs: BTreeMap<String, InputArtifact>,
    pub outputs: BTreeMap<String, OutputArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputArtifact {
    pub source: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputArtifact {
    pub path: String,
    pub destination: Option<String>,
    pub required: bool,
    pub kind: String,
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reproducibility {
    pub contract: String,
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub schema_version: u32,
    pub source: String,
    pub job: ComputeJob,
    pub target: Target,
    pub image: Option<String>,
}

impl Task {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.schema_version == 3,
            "unsupported task schema; rebuild with schema v3"
        );
        store_path(&self.source)?;
        self.job.validate()?;
        self.target.validate()?;
        ensure!(
            self.job.targets.len() == 1,
            "a built task must contain one selected target"
        );
        let summary = self
            .job
            .targets
            .get(&self.target.summary.name)
            .context("selected target missing")?;
        ensure!(
            serde_json::to_value(summary)? == serde_json::to_value(&self.target.summary)?,
            "task target summary mismatch"
        );
        ensure!(
            self.image.is_some() == (summary.executor == "oci"),
            "task image/executor mismatch"
        );
        if let Some(image) = &self.image {
            store_path(image)?;
        }
        Ok(())
    }

    pub fn load(root: &Path) -> anyhow::Result<Self> {
        let manifest = root.join("task.json");
        ensure!(
            std::fs::symlink_metadata(&manifest)
                .context("expected a built computeTasks output containing task.json")?
                .file_type()
                .is_file(),
            "task.json must be a regular file within the task output"
        );
        let task: Self = serde_json::from_slice(
            &std::fs::read(manifest)
                .context("expected a built computeTasks output containing task.json")?,
        )?;
        task.validate()?;
        Ok(task)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeContext {
    pub run_id: String,
    pub node_id: String,
    pub node_rank: u32,
    pub node_count: u32,
    pub master_addr: String,
    pub master_port: u16,
}

impl NodeContext {
    pub fn validate(&self, target: &TargetSummary) -> anyhow::Result<()> {
        ensure!(
            safe_name(&self.run_id) && safe_name(&self.node_id),
            "invalid run or node identity"
        );
        ensure!(
            self.node_count == target.resources.nodes && self.node_rank < self.node_count,
            "node context does not match task topology"
        );
        ensure!(
            !self.master_addr.is_empty()
                && self
                    .master_addr
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-:".contains(&c))
                && self.master_port > 0,
            "invalid rendezvous address or port"
        );
        Ok(())
    }

    pub fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("NIX_COMPUTE_NODE_RANK".into(), self.node_rank.to_string()),
            ("NIX_COMPUTE_NODE_COUNT".into(), self.node_count.to_string()),
            ("NIX_COMPUTE_MASTER_ADDR".into(), self.master_addr.clone()),
            (
                "NIX_COMPUTE_MASTER_PORT".into(),
                self.master_port.to_string(),
            ),
        ])
    }
}

pub fn store_path(path: &str) -> anyhow::Result<()> {
    let relative = path
        .strip_prefix("/nix/store/")
        .context("expected Nix store path")?;
    relative_path(relative)?;
    ensure!(!path.contains([',', ':']), "invalid Nix store path");
    Ok(())
}

pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub fn relative_path(path: &str) -> anyhow::Result<()> {
    ensure!(
        !path.is_empty() && !path.contains('\\') && !path.contains('\0'),
        "invalid relative artifact path {path:?}"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "artifact path must contain only relative normal components: {path}"
    );
    Ok(())
}

pub fn digest_valid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

impl ComputeJob {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.schema_version == 3,
            "unsupported schema; migrate inputs to source and rebuild with schema v3"
        );
        ensure!(safe_name(&self.name), "invalid job name");
        ensure!(
            !self.targets.is_empty(),
            "job must explicitly declare targets"
        );
        ensure!(
            self.reproducibility.contract == "environment-inputs",
            "unsupported reproducibility contract"
        );
        let mut paths = Vec::new();
        for (name, input) in &self.artifacts.inputs {
            ensure!(safe_name(name), "invalid input name {name}");
            relative_path(&input.path)?;
            store_path(&input.source).with_context(|| format!("invalid input source {name}"))?;
            paths.push(Path::new(&input.path));
        }
        for (i, left) in paths.iter().enumerate() {
            ensure!(
                !paths
                    .iter()
                    .skip(i + 1)
                    .any(|right| left.starts_with(right) || right.starts_with(left)),
                "input paths overlap"
            );
        }
        for (name, output) in &self.artifacts.outputs {
            ensure!(safe_name(name), "invalid output name {name}");
            relative_path(&output.path)?;
            ensure!(
                matches!(output.kind.as_str(), "file" | "directory"),
                "invalid output kind"
            );
            ensure!(
                matches!(output.scope.as_str(), "leader" | "per-node"),
                "invalid output scope"
            );
        }
        for (name, target) in &self.targets {
            ensure!(name == &target.name, "target name mismatch");
            target.validate()?;
        }
        Ok(())
    }

    pub fn common_identity(&self) -> Value {
        serde_json::json!({"schema_version": self.schema_version, "name": self.name,
            "artifacts": self.artifacts, "parameters": self.parameters, "reproducibility": self.reproducibility})
    }
}

impl TargetSummary {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(safe_name(&self.name), "invalid target name");
        ensure!(
            matches!(
                self.system.as_str(),
                "x86_64-linux" | "aarch64-linux" | "aarch64-darwin"
            ),
            "unsupported target system"
        );
        ensure!(
            matches!(self.executor.as_str(), "oci" | "native"),
            "unsupported executor"
        );
        ensure!(
            self.executor != "oci" || self.system.ends_with("-linux"),
            "OCI requires Linux"
        );
        ensure!(
            matches!(self.execution.network.as_str(), "none" | "host"),
            "unsupported network policy"
        );
        ensure!(
            matches!(self.execution.isolation.as_str(), "none" | "container"),
            "unsupported isolation policy"
        );
        ensure!(
            self.execution.timeout_seconds != Some(0),
            "timeout must be positive"
        );
        ensure!(self.resources.nodes > 0, "nodes must be positive");
        ensure!(
            self.resources.nodes == 1 || self.execution.network == "host",
            "multi-node tasks require network = host for peer connectivity"
        );
        ensure!(
            self.resources.cpu_cores != Some(0) && self.resources.memory_mib != Some(0),
            "resources must be positive"
        );
        for (key, value) in &self.execution.env {
            ensure!(
                !key.is_empty()
                    && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    && !key.as_bytes()[0].is_ascii_digit(),
                "invalid environment key"
            );
            ensure!(
                !key.starts_with("NIX_COMPUTE_") && !value.contains('\0'),
                "reserved or invalid environment variable {key}"
            );
        }
        let a = &self.accelerator;
        ensure!(a.count > 0, "accelerator count must be positive");
        match a.id.as_str() {
            "cpu" => ensure!(
                a.count == 1
                    && a.min_memory_mib == 0
                    && a.architectures.is_empty()
                    && a.topology.is_empty()
                    && a.driver_range.is_none()
                    && a.runtime_version.is_none(),
                "use resources for CPU requirements"
            ),
            "metal" => ensure!(
                self.system == "aarch64-darwin" && self.executor == "native",
                "Metal requires native Apple Silicon"
            ),
            "tpu" => ensure!(
                self.system.ends_with("-linux") && self.executor == "native",
                "TPU requires native Linux"
            ),
            "cuda" | "cann" => ensure!(
                self.system.ends_with("-linux") && self.executor == "oci",
                "backend requires Linux OCI"
            ),
            "rocm" | "oneapi" => ensure!(
                self.system == "x86_64-linux" && self.executor == "oci",
                "backend requires x86_64 Linux OCI"
            ),
            _ => anyhow::bail!("unsupported accelerator {}", a.id),
        }
        if a.id != "cpu" {
            let range = a
                .driver_range
                .as_deref()
                .context("accelerators require an explicit driver_range")?;
            semver::VersionReq::parse(range).context("invalid driver_range (use semver syntax)")?;
            ensure!(
                a.runtime_version.as_ref().is_some_and(|v| !v.is_empty()),
                "accelerators require an exact runtime_version"
            );
            ensure!(
                !a.architectures.is_empty(),
                "accelerators require explicit architectures"
            );
        }
        Ok(())
    }
}

impl Target {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.summary.validate()?;
        store_path(&self.program)?;
        ensure!(
            self.entrypoint
                .first()
                .is_some_and(|s| s.starts_with("/nix/store/"))
                && self.entrypoint.iter().all(|s| !s.contains('\0')),
            "entrypoint must use a Nix store executable"
        );
        store_path(&self.entrypoint[0])?;
        if let Some(probe) = &self.probe {
            store_path(probe)?;
        }
        for path in &self.runtime_paths {
            store_path(path)?;
        }
        ensure!(
            (self.summary.executor == "oci") == self.image_output.is_some(),
            "image/executor mismatch"
        );
        if self.summary.accelerator.id != "cpu" {
            ensure!(
                self.probe
                    .as_ref()
                    .is_some_and(|s| s.starts_with("/nix/store/"))
                    && self.probe_output.is_some(),
                "accelerator requires a locked probe derivation"
            );
            ensure!(
                !self.runtime_paths.is_empty()
                    && self
                        .runtime_paths
                        .iter()
                        .all(|p| p.starts_with("/nix/store/")),
                "accelerator requires locked runtimePackages"
            );
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "test-fixtures"))]
pub fn fixture() -> (ComputeJob, Target) {
    let value: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/cpu.json")).unwrap();
    (
        serde_json::from_value(value["job"].clone()).unwrap(),
        serde_json::from_value(value["target"].clone()).unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_manifest_must_not_follow_external_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("task");
        std::fs::create_dir(&root).unwrap();
        let (job, target) = fixture();
        let task = Task {
            schema_version: 3,
            source: "/nix/store/fixture-source".into(),
            job,
            target,
            image: None,
        };
        let external = dir.path().join("external.json");
        std::fs::write(&external, serde_json::to_vec(&task).unwrap()).unwrap();
        std::os::unix::fs::symlink(&external, root.join("task.json")).unwrap();
        assert!(Task::load(&root)
            .unwrap_err()
            .to_string()
            .contains("regular file"));
        std::fs::remove_file(root.join("task.json")).unwrap();
        std::fs::copy(external, root.join("task.json")).unwrap();
        Task::load(&root).unwrap();
    }

    #[test]
    fn artifact_paths_are_relative_components() {
        for path in ["../x", "a/../../b", "/tmp/x", "", "a\\b", "./x"] {
            assert!(relative_path(path).is_err(), "{path}");
        }
        for path in ["checkpoint", "epoch.1/model.bin", "file..bin"] {
            assert!(relative_path(path).is_ok());
        }
    }

    #[test]
    fn v3_contract_rejects_ignored_or_incompatible_requirements() {
        let (mut job, mut target) = fixture();
        job.validate().unwrap();
        target.validate().unwrap();
        let mut unknown = serde_json::to_value(&target).unwrap();
        unknown["unsupported_requirement"] = true.into();
        assert!(serde_json::from_value::<Target>(unknown).is_err());
        job.schema_version = 1;
        assert!(job.validate().unwrap_err().to_string().contains("migrate"));
        target.summary.accelerator.id = "metal".into();
        assert!(target.validate().is_err());
        target.summary.accelerator.id = "cpu".into();
        target
            .summary
            .execution
            .env
            .insert("NIX_COMPUTE_OUTPUTS".into(), "/escape".into());
        assert!(target.validate().is_err());
    }

    #[test]
    fn topology_and_node_context_are_validated_together() {
        let (_, mut target) = fixture();
        target.summary.resources.nodes = 2;
        target.validate().unwrap();
        let mut node = NodeContext {
            run_id: "run".into(),
            node_id: "worker".into(),
            node_rank: 1,
            node_count: 2,
            master_addr: "10.0.0.1".into(),
            master_port: 29500,
        };
        node.validate(&target.summary).unwrap();
        assert_eq!(node.environment()["NIX_COMPUTE_NODE_COUNT"], "2");
        node.node_rank = 2;
        assert!(node.validate(&target.summary).is_err());
        node.node_rank = 0;
        node.node_count = 1;
        assert!(node.validate(&target.summary).is_err());
        target.summary.execution.network = "none".into();
        assert!(target.validate().is_err());
        target.summary.resources.nodes = 0;
        assert!(target.validate().is_err());
    }

    #[test]
    fn task_rejects_unselected_target_old_schema_and_escaping_inputs() {
        let (job, target) = fixture();
        let mut task = Task {
            schema_version: 3,
            source: "/nix/store/fixture-source".into(),
            job,
            target,
            image: None,
        };
        task.validate().unwrap();
        task.schema_version = 2;
        assert!(task.validate().is_err());
        task.schema_version = 3;
        task.target.summary.resources.nodes = 2;
        assert!(task.validate().is_err());
        task.target.summary.resources.nodes = 1;
        task.job.artifacts.inputs.insert(
            "data".into(),
            InputArtifact {
                source: "/nix/store/../secret".into(),
                path: "data".into(),
            },
        );
        assert!(task.validate().is_err());
    }
}
