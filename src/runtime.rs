use crate::{
    accelerator::Launch,
    model::{ComputeJob, Target, TargetSummary},
    nix, process,
};
use anyhow::{ensure, Context};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::AtomicBool,
};

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub system: String,
    pub cpu_cores: usize,
    pub memory_mib: u64,
    pub container_runtime: Option<String>,
    pub container_runtime_version: Option<String>,
    pub oci_cpu_limits: bool,
    pub oci_memory_limits: bool,
}

pub fn detect_capabilities() -> anyhow::Result<Capabilities> {
    let system = nix::current_system()?;
    let memory_mib = if system.ends_with("-darwin") {
        String::from_utf8(process::capture(
            Command::new("sysctl").args(["-n", "hw.memsize"]),
            5,
        )?)?
        .trim()
        .parse::<u64>()?
            / 1024
            / 1024
    } else {
        fs::read_to_string("/proc/meminfo")?
            .lines()
            .find_map(|line| {
                line.strip_prefix("MemTotal:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
            .context("cannot determine host memory")?
            / 1024
    };
    let mut container_runtime = None;
    let mut container_runtime_version = None;
    let mut oci_cpu_limits = false;
    let mut oci_memory_limits = false;
    for runtime in ["podman", "docker"] {
        if let Ok((cpu, memory)) = runtime_limits(runtime) {
            let version = process::capture(Command::new(runtime).arg("--version"), 5)?;
            container_runtime = Some(runtime.into());
            container_runtime_version = Some(String::from_utf8(version)?.trim().into());
            oci_cpu_limits = cpu;
            oci_memory_limits = memory;
            break;
        }
    }
    Ok(Capabilities {
        system,
        cpu_cores: std::thread::available_parallelism()?.get(),
        memory_mib,
        container_runtime,
        container_runtime_version,
        oci_cpu_limits,
        oci_memory_limits,
    })
}

fn runtime_limits(runtime: &str) -> anyhow::Result<(bool, bool)> {
    if runtime == "podman" {
        let info: Value = serde_json::from_slice(&process::capture(
            Command::new(runtime).args(["info", "--format", "json"]),
            5,
        )?)?;
        ensure!(
            info["host"]["serviceIsRemote"] == false,
            "remote Podman execution is unsupported"
        );
        let host = &info["host"];
        let controllers = host["cgroupControllers"].as_array();
        let supports = |name: &str| {
            host["cgroupVersion"] == "v2"
                && controllers.is_some_and(|c| c.iter().any(|v| v == name))
        };
        Ok((supports("cpu"), supports("memory")))
    } else {
        let explicit_context = std::env::var("DOCKER_CONTEXT").is_ok_and(|value| !value.is_empty());
        let host_override = std::env::var("DOCKER_HOST")
            .ok()
            .filter(|_| !explicit_context);
        let endpoint = if let Some(host) = host_override {
            host
        } else {
            serde_json::from_slice::<String>(&process::capture(
                Command::new(runtime).args([
                    "context",
                    "inspect",
                    "--format",
                    "{{json .Endpoints.docker.Host}}",
                ]),
                5,
            )?)?
        };
        ensure!(
            endpoint.starts_with("unix://"),
            "remote Docker execution is unsupported"
        );
        let info: Value = serde_json::from_slice(&process::capture(
            Command::new(runtime).args(["info", "--format", "{{json .}}"]),
            5,
        )?)?;
        Ok((
            info["CpuCfsQuota"] == true,
            info["MemoryLimit"] == true && info["SwapLimit"] == true,
        ))
    }
}

pub fn check_host(target: &TargetSummary, caps: &Capabilities) -> anyhow::Result<()> {
    ensure!(
        target.system == caps.system,
        "target system {} differs from host {}",
        target.system,
        caps.system
    );
    ensure!(
        target
            .resources
            .cpu_cores
            .is_none_or(|n| n as usize <= caps.cpu_cores),
        "insufficient CPU cores"
    );
    ensure!(
        target
            .resources
            .memory_mib
            .is_none_or(|n| n <= caps.memory_mib),
        "insufficient host memory"
    );
    match target.executor.as_str() {
        "oci" => {
            ensure!(
                caps.container_runtime.is_some(),
                "no working local Podman/Docker service"
            );
            if target.resources.enforce {
                ensure!(
                    target.resources.cpu_cores.is_none() || caps.oci_cpu_limits,
                    "OCI runtime cannot confirm CPU quota enforcement"
                );
                ensure!(
                    target.resources.memory_mib.is_none() || caps.oci_memory_limits,
                    "OCI runtime cannot confirm memory/swap limit enforcement"
                );
            }
        }
        "native" => {
            ensure!(target.execution.network == "host", "native executor cannot enforce network isolation; explicitly select execution.network = host");
            ensure!(
                target.execution.isolation == "none",
                "native executor cannot enforce container isolation"
            );
            ensure!(
                !target.resources.enforce
                    || (target.resources.cpu_cores.is_none()
                        && target.resources.memory_mib.is_none()),
                "native executor cannot enforce CPU/memory limits"
            );
        }
        _ => anyhow::bail!("unknown executor"),
    }
    Ok(())
}

pub fn select(candidates: &BTreeMap<String, Result<(), String>>) -> anyhow::Result<String> {
    let fitting: Vec<_> = candidates
        .iter()
        .filter(|(_, status)| status.is_ok())
        .map(|(name, _)| name.clone())
        .collect();
    if fitting.len() == 1 {
        return Ok(fitting[0].clone());
    }
    let detail = candidates
        .iter()
        .map(|(name, status)| {
            format!(
                "{name}: {}",
                status
                    .as_ref()
                    .err()
                    .map(String::as_str)
                    .unwrap_or("compatible")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    anyhow::bail!(
        "{} compatible targets; select --target explicitly. {detail}",
        fitting.len()
    )
}

pub struct Workspace {
    pub root: PathBuf,
    pub inputs: PathBuf,
    pub outputs: PathBuf,
    pub work: PathBuf,
    pub tmp: PathBuf,
}
impl Workspace {
    pub fn create(root: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        ensure!(
            !root.to_string_lossy().contains([',', ':']),
            "workspace path cannot contain comma or colon"
        );
        let workspace = Self {
            inputs: root.join("inputs"),
            outputs: root.join("outputs"),
            work: root.join("work"),
            tmp: root.join("tmp"),
            root,
        };
        for path in [
            &workspace.inputs,
            &workspace.outputs,
            &workspace.work,
            &workspace.tmp,
        ] {
            fs::create_dir(path)?;
        }
        Ok(workspace)
    }
}

fn environment(
    job: &ComputeJob,
    target: &Target,
    launch: &Launch,
    workspace: &Workspace,
    oci: bool,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut env = target.summary.execution.env.clone();
    // Allocation settings take precedence over job environment variables.
    env.extend(launch.env.clone());
    env.insert(
        "NIX_COMPUTE_INPUTS".into(),
        if oci {
            "/inputs".into()
        } else {
            workspace.inputs.display().to_string()
        },
    );
    env.insert(
        "NIX_COMPUTE_OUTPUTS".into(),
        if oci {
            "/outputs".into()
        } else {
            workspace.outputs.display().to_string()
        },
    );
    env.insert(
        "NIX_COMPUTE_PARAMETERS".into(),
        serde_json::to_string(&job.parameters)?,
    );
    if let Some(seed) = job.reproducibility.seed {
        env.insert("NIX_COMPUTE_SEED".into(), seed.to_string());
    }
    env.insert(
        "HOME".into(),
        if oci {
            "/tmp".into()
        } else {
            workspace.tmp.display().to_string()
        },
    );
    env.insert(
        "TMPDIR".into(),
        if oci {
            "/tmp".into()
        } else {
            workspace.tmp.display().to_string()
        },
    );
    Ok(env)
}

pub fn run_native(
    job: &ComputeJob,
    target: &Target,
    launch: &Launch,
    workspace: &Workspace,
    cancelled: &AtomicBool,
) -> anyhow::Result<i32> {
    let mut command = Command::new(&target.entrypoint[0]);
    command
        .args(&target.entrypoint[1..])
        .current_dir(&workspace.work)
        .env_clear()
        .env("PATH", "")
        .envs(environment(job, target, launch, workspace, false)?);
    command
        .stdout(fs::File::create(workspace.root.join("stdout.log"))?)
        .stderr(fs::File::create(workspace.root.join("stderr.log"))?);
    process::run(
        &mut command,
        target.summary.execution.timeout_seconds,
        cancelled,
    )
}

fn archive_reader(path: &Path) -> anyhow::Result<Box<dyn Read>> {
    let mut reader = std::io::BufReader::new(fs::File::open(path)?);
    if reader.fill_buf()?.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(flate2::read::GzDecoder::new(reader)))
    } else {
        Ok(Box::new(reader))
    }
}

fn archive_file(path: &Path, name: &str) -> anyhow::Result<Vec<u8>> {
    for entry in tar::Archive::new(archive_reader(path)?).entries()? {
        let entry = entry?;
        if entry.path()?.as_ref() == Path::new(name) {
            ensure!(
                entry.size() <= 4 * 1024 * 1024,
                "oversized image manifest/config"
            );
            let mut data = Vec::new();
            entry.take(4 * 1024 * 1024).read_to_end(&mut data)?;
            return Ok(data);
        }
    }
    anyhow::bail!("image archive lacks {name}")
}

pub fn load_image(archive: &Path, runtime: &str) -> anyhow::Result<String> {
    let manifest: Value = serde_json::from_slice(&archive_file(archive, "manifest.json")?)?;
    let images = manifest
        .as_array()
        .context("invalid Docker archive manifest")?;
    ensure!(images.len() == 1, "expected exactly one image in archive");
    let config_path = images[0]["Config"]
        .as_str()
        .context("image config missing")?;
    let image_id = format!(
        "sha256:{}",
        crate::canonical::sha256_bytes(&archive_file(archive, config_path)?)
    );
    let output = Command::new(runtime)
        .args(["load", "-i"])
        .arg(archive)
        .output()?;
    ensure!(
        output.status.success(),
        "image load failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inspected = process::capture(
        Command::new(runtime).args(["image", "inspect", "--format", "{{.Id}}", &image_id]),
        15,
    )?;
    ensure!(
        normalize_image_id(String::from_utf8(inspected)?.trim())? == image_id,
        "loaded image does not match archive config digest"
    );
    Ok(image_id)
}

fn normalize_image_id(value: &str) -> anyhow::Result<String> {
    let digest = value.strip_prefix("sha256:").unwrap_or(value);
    ensure!(
        crate::model::digest_valid(digest),
        "runtime returned an invalid image ID"
    );
    Ok(format!("sha256:{}", digest.to_ascii_lowercase()))
}

pub fn container_args(
    job: &ComputeJob,
    target: &Target,
    launch: &Launch,
    workspace: &Workspace,
    runtime: &str,
    name: &str,
    image: &str,
) -> anyhow::Result<Vec<String>> {
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let user = if uid == 0 {
        "65532:65532".into()
    } else {
        format!("{uid}:{gid}")
    };
    let mut args = vec![
        "create".into(),
        format!("--name={name}"),
        "--read-only".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges".into(),
        format!("--user={user}"),
        "--workdir=/workspace".into(),
        "--pids-limit=4096".into(),
    ];
    if runtime == "podman" && uid != 0 {
        args.push("--userns=keep-id".into());
    }
    args.push(format!("--network={}", target.summary.execution.network));
    if target.summary.resources.enforce {
        if let Some(cpu) = target.summary.resources.cpu_cores {
            args.push(format!("--cpus={cpu}"));
        }
        if let Some(memory) = target.summary.resources.memory_mib {
            args.push(format!("--memory={memory}m"));
            args.push(format!("--memory-swap={memory}m"));
        }
    }
    for (source, destination, readonly) in [
        (&workspace.inputs, "/inputs", true),
        (&workspace.outputs, "/outputs", false),
        (&workspace.work, "/workspace", false),
        (&workspace.tmp, "/tmp", false),
    ] {
        args.push(format!(
            "--mount=type=bind,src={},dst={destination}{}",
            source.display(),
            if readonly { ",readonly" } else { "" }
        ));
    }
    for (key, value) in environment(job, target, launch, workspace, true)? {
        args.push("--env".into());
        args.push(format!("{key}={value}"));
    }
    args.extend(launch.args.clone());
    if !launch.devices.is_empty() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let mut groups = std::collections::BTreeSet::new();
        for device in &launch.devices {
            for node in &device.device_nodes {
                let metadata =
                    fs::metadata(node).with_context(|| format!("missing device node {node}"))?;
                ensure!(
                    metadata.file_type().is_char_device(),
                    "not a character device: {node}"
                );
                groups.insert(metadata.gid());
            }
        }
        if runtime == "podman" && uid != 0 && !groups.is_empty() {
            args.push("--group-add=keep-groups".into());
        } else {
            args.extend(groups.into_iter().map(|gid| format!("--group-add={gid}")));
        }
    }
    // Explicitly replace the image entrypoint; arguments are supplied exactly once.
    args.push(format!("--entrypoint={}", target.entrypoint[0]));
    args.push(image.into());
    args.extend(target.entrypoint[1..].iter().cloned());
    Ok(args)
}

struct Container<'a> {
    runtime: &'a str,
    name: String,
    removed: bool,
}

#[derive(Debug)]
pub struct CleanupFailure {
    runtime: String,
    name: String,
    cause: String,
}
impl std::fmt::Display for CleanupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unable to remove {} container {}: {}",
            self.runtime, self.name, self.cause
        )
    }
}
impl std::error::Error for CleanupFailure {}
impl Container<'_> {
    fn remove(&mut self) -> anyhow::Result<()> {
        process::capture(
            Command::new(self.runtime).args(["rm", "--force", &self.name]),
            30,
        )
        .map_err(|error| CleanupFailure {
            runtime: self.runtime.into(),
            name: self.name.clone(),
            cause: format!("{error:#}"),
        })?;
        self.removed = true;
        Ok(())
    }
}
impl Drop for Container<'_> {
    fn drop(&mut self) {
        if !self.removed {
            let _ = process::capture(
                Command::new(self.runtime).args(["rm", "--force", &self.name]),
                30,
            );
        }
    }
}

pub fn prepare_oci_launch(
    job: &ComputeJob,
    target: &Target,
    launch: &Launch,
    workspace: &Workspace,
    runtime: &str,
    image: &str,
) -> anyhow::Result<Launch> {
    if target.summary.accelerator.id == "cpu" {
        return Ok(launch.clone());
    }
    prepare_container_workspace(workspace)?;
    let mut probe_target = target.clone();
    probe_target.entrypoint = vec![target.probe.clone().context("missing OCI probe")?];
    for key in [
        "CUDA_VISIBLE_DEVICES",
        "HIP_VISIBLE_DEVICES",
        "ROCR_VISIBLE_DEVICES",
        "ASCEND_RT_VISIBLE_DEVICES",
        "ONEAPI_DEVICE_SELECTOR",
        "ZE_AFFINITY_MASK",
    ] {
        probe_target.summary.execution.env.remove(key);
    }
    let mut unrestricted_env = launch.clone();
    unrestricted_env.env.clear();
    let name = format!("nix-compute-probe-{}", uuid::Uuid::new_v4());
    let args = container_args(
        job,
        &probe_target,
        &unrestricted_env,
        workspace,
        runtime,
        &name,
        image,
    )?;
    let mut container = Container {
        runtime,
        name,
        removed: false,
    };
    process::capture(Command::new(runtime).args(&args), 30)?;
    let output = process::capture(
        Command::new(runtime).args(["start", "--attach", &container.name]),
        30,
    );
    container.remove()?;
    let inventory = crate::accelerator::parse_inventory(&output?, &target.summary.accelerator.id)?;
    remap_launch(target, launch, &inventory)
}

fn remap_launch(
    target: &Target,
    launch: &Launch,
    inventory: &crate::accelerator::Inventory,
) -> anyhow::Result<Launch> {
    let mut host_ids = launch.devices.iter().map(|d| &d.id).collect::<Vec<_>>();
    let mut local_ids = inventory.devices.iter().map(|d| &d.id).collect::<Vec<_>>();
    host_ids.sort();
    local_ids.sort();
    ensure!(
        host_ids == local_ids,
        "container-visible devices differ from the allocation"
    );
    let matching = crate::accelerator::matching(&target.summary.accelerator, inventory)?;
    let remapped =
        crate::accelerator::adapter(&inventory.backend)?.configure(&matching, inventory)?;
    let mut result = launch.clone();
    result.env = remapped.env;
    Ok(result)
}

fn prepare_container_workspace(workspace: &Workspace) -> anyhow::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        use std::os::unix::ffi::OsStrExt;
        for path in [&workspace.outputs, &workspace.work, &workspace.tmp] {
            let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
            ensure!(
                unsafe { libc::chown(path.as_ptr(), 65532, 65532) } == 0,
                "cannot prepare non-root workspace ownership"
            );
        }
    }
    Ok(())
}

pub fn run_container(
    job: &ComputeJob,
    target: &Target,
    launch: &Launch,
    workspace: &Workspace,
    runtime: &str,
    image: &str,
    cancelled: &AtomicBool,
) -> anyhow::Result<i32> {
    prepare_container_workspace(workspace)?;
    let name = format!("nix-compute-{}", uuid::Uuid::new_v4());
    let args = container_args(job, target, launch, workspace, runtime, &name, image)?;
    let mut container = Container {
        runtime,
        name,
        removed: false,
    };
    process::capture(Command::new(runtime).args(&args), 30)?;
    let mut command = Command::new(runtime);
    command
        .args(["start", "--attach", &container.name])
        .stdin(Stdio::null())
        .stdout(fs::File::create(workspace.root.join("stdout.log"))?)
        .stderr(fs::File::create(workspace.root.join("stderr.log"))?);
    let result = process::run(
        &mut command,
        target.summary.execution.timeout_seconds,
        cancelled,
    );
    container.remove().context(
        "failed to remove workload container; inspect the runtime before releasing this node",
    )?;
    result
}

pub fn output_path(root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    crate::model::relative_path(relative)?;
    let path = root.join(relative).canonicalize()?;
    ensure!(
        path.starts_with(root.canonicalize()?) && path.is_file(),
        "output must be a regular file inside the output directory"
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn docker_and_podman_image_ids_use_the_same_digest() {
        let digest = "a".repeat(64);
        assert_eq!(
            normalize_image_id(&digest).unwrap(),
            normalize_image_id(&format!("sha256:{digest}")).unwrap()
        );
        assert!(normalize_image_id("nix-compute:job").is_err());
    }
    #[test]
    fn ambiguity_and_no_match_are_errors() {
        let mut candidates = BTreeMap::from([("cpu".into(), Ok(())), ("cuda".into(), Ok(()))]);
        assert!(select(&candidates)
            .unwrap_err()
            .to_string()
            .contains("2 compatible"));
        candidates.insert("cuda".into(), Err("no GPU".into()));
        assert_eq!(select(&candidates).unwrap(), "cpu");
        candidates.insert("cpu".into(), Err("no runtime".into()));
        assert!(select(&candidates)
            .unwrap_err()
            .to_string()
            .contains("no GPU"));
    }
    #[test]
    fn artifact_symlinks_cannot_escape_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("escape")).unwrap();
        assert!(output_path(dir.path(), "escape").is_err());
    }

    fn shell() -> String {
        String::from_utf8(
            Command::new("sh")
                .args(["-c", "command -v sh"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .into()
    }

    #[test]
    fn native_workspace_and_resource_contract() {
        let (job, mut target) = crate::model::fixture();
        let caps = Capabilities {
            system: "x86_64-linux".into(),
            cpu_cores: 4,
            memory_mib: 8192,
            container_runtime: None,
            container_runtime_version: None,
            oci_cpu_limits: false,
            oci_memory_limits: false,
        };
        check_host(&target.summary, &caps).unwrap();
        target.summary.resources.cpu_cores = Some(2);
        target.summary.resources.enforce = true;
        assert!(check_host(&target.summary, &caps).is_err());
        target.summary.resources.enforce = false;
        target.summary.execution.network = "none".into();
        assert!(check_host(&target.summary, &caps).is_err());
        target.summary.execution.network = "host".into();
        target.summary.execution.isolation = "container".into();
        assert!(check_host(&target.summary, &caps).is_err());
        target.summary.execution.isolation = "none".into();
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::create(&dir.path().join("run")).unwrap();
        target.entrypoint = vec![
            shell(),
            "-c".into(),
            "printf '%s' \"$NIX_COMPUTE_PARAMETERS\" > \"$NIX_COMPUTE_OUTPUTS/checkpoint\"".into(),
        ];
        assert_eq!(
            run_native(
                &job,
                &target,
                &Launch::default(),
                &workspace,
                &AtomicBool::new(false)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            fs::read_to_string(workspace.outputs.join("checkpoint")).unwrap(),
            "{\"steps\":2}"
        );
    }

    #[test]
    fn oci_arguments_do_not_duplicate_entrypoint() {
        let (job, mut target) = crate::model::fixture();
        target.summary.execution.network = "none".into();
        target.summary.resources.cpu_cores = Some(2);
        target.summary.resources.memory_mib = Some(512);
        target.summary.resources.enforce = true;
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::create(&dir.path().join("run")).unwrap();
        let args = container_args(
            &job,
            &target,
            &Launch::default(),
            &workspace,
            "docker",
            "container",
            "sha256:test",
        )
        .unwrap();
        assert_eq!(
            args.iter().filter(|a| a.contains("/bin/trainer")).count(),
            1
        );
        assert!(args.ends_with(&["sha256:test".into(), "--steps".into(), "2".into()]));
        for expected in [
            "--network=none",
            "--cpus=2",
            "--memory=512m",
            "--memory-swap=512m",
            "--read-only",
            "--cap-drop=ALL",
        ] {
            assert!(args.iter().any(|a| a == expected));
        }
        assert!(args.iter().any(|a| a == "NIX_COMPUTE_OUTPUTS=/outputs"));
    }

    #[test]
    fn oci_cleanup_runs_after_timeout_and_failure() {
        use std::os::unix::fs::PermissionsExt;
        for timeout in [false, true] {
            let (job, mut target) = crate::model::fixture();
            let dir = tempfile::tempdir().unwrap();
            let workspace = Workspace::create(&dir.path().join("run")).unwrap();
            let marker = dir.path().join("removed");
            let runtime = dir.path().join("runtime");
            let start = if timeout { "sleep 5" } else { "exit 9" };
            fs::write(&runtime, format!("#!{}\ncase \"$1\" in\ncreate) exit 0;;\nstart) {start};;\nrm) touch '{}' ;;\nesac\n", shell(), marker.display())).unwrap();
            fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
            target.summary.execution.timeout_seconds = Some(1);
            let code = run_container(
                &job,
                &target,
                &Launch::default(),
                &workspace,
                runtime.to_str().unwrap(),
                "sha256:test",
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(code, if timeout { 124 } else { 9 });
            assert!(marker.exists());
        }
    }

    #[test]
    fn container_device_ordinals_are_remapped_by_identity() {
        let (_, mut target) = crate::model::fixture();
        let fixtures: Value =
            serde_json::from_str(include_str!("../tests/fixtures/inventories.json")).unwrap();
        let mut host = crate::accelerator::parse_inventory(
            &serde_json::to_vec(&fixtures["rocm"]).unwrap(),
            "rocm",
        )
        .unwrap();
        host.devices[0].index = 3;
        let device = &host.devices[0];
        target.summary.accelerator = crate::model::Accelerator {
            id: "rocm".into(),
            count: 1,
            min_memory_mib: 1,
            architectures: vec![device.architecture.clone()],
            driver_range: Some(">=0.0.0".into()),
            runtime_version: Some(device.runtime_version.clone()),
            topology: BTreeMap::new(),
        };
        let launch = crate::accelerator::adapter("rocm")
            .unwrap()
            .configure(&host.devices, &host)
            .unwrap();
        let mut inside = host.clone();
        inside.devices[0].index = 0;
        let mapped = remap_launch(&target, &launch, &inside).unwrap();
        assert_eq!(mapped.env["ROCR_VISIBLE_DEVICES"], "0");
        assert_eq!(mapped.devices[0].index, 3);
        inside.devices[0].id = "unexpected".into();
        assert!(remap_launch(&target, &launch, &inside).is_err());
    }
}
