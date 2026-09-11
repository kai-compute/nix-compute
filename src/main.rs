mod accelerator;
mod attestation;
mod canonical;
mod cas;
mod model;
mod nix;
mod process;
mod runtime;

use anyhow::{ensure, Context};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Parser)]
#[command(
    name = "nix-compute",
    version,
    about = "Execute explicitly declared targets from a locked Nix Flake"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Inspect {
        selector: String,
        #[arg(long)]
        target: Option<String>,
    },
    Validate {
        selector: String,
        #[arg(long)]
        target: Option<String>,
    },
    Build {
        selector: String,
        #[arg(long)]
        target: Option<String>,
    },
    Run {
        selector: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        signing_key: PathBuf,
        #[arg(long, default_value = ".nix-compute/runs")]
        state_dir: PathBuf,
    },
    Capabilities {
        selector: Option<String>,
        #[arg(long, requires = "selector")]
        target: Option<String>,
    },
    Verify {
        attestation: PathBuf,
        trust_store: PathBuf,
    },
    Keygen {
        output: PathBuf,
        #[arg(long, default_value = "local-center")]
        center_id: String,
        #[arg(long, default_value = "local-key")]
        key_id: String,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Inspect { selector, target } | Command::Validate { selector, target } => {
            let (selector, _) = nix::Selector::parse(&selector)?.snapshot()?;
            let job = selector.job()?;
            if let Some(target) = target {
                ensure!(job.targets.contains_key(&target), "unknown target {target}");
                print_json(&selector.target(&target)?)?;
            } else {
                print_json(&job)?;
            }
        }
        Command::Build { selector, target } => {
            let (selector, _) = nix::Selector::parse(&selector)?.snapshot()?;
            let job = selector.job()?;
            let name = named_target(&job, target.as_deref())?;
            let target = selector.target(&name)?;
            if target.summary.executor == "native" {
                for output in [&target.probe_output, &target.runtime_output]
                    .into_iter()
                    .flatten()
                {
                    selector.build(output)?;
                }
            }
            let output = target
                .image_output
                .as_deref()
                .unwrap_or(&target.program_output);
            println!("{}", selector.build(output)?);
        }
        Command::Run {
            selector,
            target,
            signing_key,
            state_dir,
        } => run(&selector, target.as_deref(), &signing_key, &state_dir)?,
        Command::Capabilities { selector, target } => {
            let caps = runtime::detect_capabilities()?;
            if let Some(selector) = selector {
                let (selector, _) = nix::Selector::parse(&selector)?.snapshot()?;
                let job = selector.job()?;
                let name = named_target(&job, target.as_deref())?;
                runtime::check_host(&job.targets[&name], &caps)?;
                let target = selector.target(&name)?;
                let inventory = probe(&selector, &target)?;
                print_json(&json!({"host": caps, "inventory": inventory}))?;
            } else {
                print_json(&caps)?;
            }
        }
        Command::Keygen {
            output,
            center_id,
            key_id,
        } => {
            attestation::generate_key(&output, &center_id, &key_id)?;
            println!("wrote {}", output.display());
        }
        Command::Verify {
            attestation,
            trust_store,
        } => {
            let value = serde_json::from_slice(&fs::read(attestation)?)?;
            println!(
                "valid payload: {}",
                attestation::verify(&value, &trust_store)?
            );
        }
    }
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn named_target(job: &model::ComputeJob, explicit: Option<&str>) -> anyhow::Result<String> {
    if let Some(name) = explicit {
        ensure!(job.targets.contains_key(name), "unknown target {name}");
        return Ok(name.into());
    }
    ensure!(
        job.targets.len() == 1,
        "--target is required; declared targets: {}",
        job.targets.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(job.targets.keys().next().unwrap().clone())
}

fn probe(
    selector: &nix::Selector,
    target: &model::Target,
) -> anyhow::Result<accelerator::Inventory> {
    if let Some(output) = &target.probe_output {
        selector.build(output)?;
    }
    let backend = accelerator::adapter(&target.summary.accelerator.id)?;
    backend.probe(target)
}

fn run(
    raw: &str,
    explicit: Option<&str>,
    signing_key: &Path,
    state_dir: &Path,
) -> anyhow::Result<()> {
    // Validate and retain the key before doing any expensive or stateful work.
    let signer = attestation::SigningIdentity::load(signing_key)?;
    let cancellation = process::Cancellation::install()?;
    let (selector, source) = nix::Selector::parse(raw)?.snapshot()?;
    let job = selector.job()?;
    let caps = runtime::detect_capabilities()?;
    let mut prepared = BTreeMap::new();
    let mut candidates = BTreeMap::new();
    if let Some(name) = explicit {
        ensure!(job.targets.contains_key(name), "unknown target {name}");
    }
    for (name, summary) in &job.targets {
        if explicit.is_some_and(|chosen| chosen != name) {
            continue;
        }
        let result = (|| -> anyhow::Result<()> {
            runtime::check_host(summary, &caps)?;
            let target = selector.target(name)?;
            let inventory = probe(&selector, &target)?;
            let matching = accelerator::matching(&target.summary.accelerator, &inventory)?;
            let devices = if target.summary.accelerator.id == "cpu" {
                &matching[..]
            } else {
                &matching[..target.summary.accelerator.count as usize]
            };
            accelerator::adapter(&target.summary.accelerator.id)?.configure(devices, &inventory)?;
            prepared.insert(name.clone(), (target, inventory));
            Ok(())
        })();
        candidates.insert(name.clone(), result.map_err(|e| format!("{e:#}")));
    }
    let name = runtime::select(&candidates)?;
    let (target, inventory) = prepared.remove(&name).unwrap();
    let mut allocation = accelerator::allocate(&target, &inventory, &accelerator::lock_root())?;
    let program = selector.build(&target.program_output)?;
    ensure!(
        program == target.program,
        "program metadata differs from built output"
    );
    let mut roots = vec![program];
    if let Some(output) = &target.runtime_output {
        roots.push(selector.build(output)?);
    }
    if let Some(output) = &target.probe_output {
        roots.push(selector.build(output)?);
    }
    let mut archive = None;
    let mut image = None;
    if let Some(output) = &target.image_output {
        let path = selector.build(output)?;
        image = Some(runtime::load_image(
            Path::new(&path),
            caps.container_runtime.as_deref().unwrap(),
        )?);
        archive = Some(json!({"path": path, "sha256": cas::sha256_file(Path::new(&path))?}));
        roots.push(path);
    }
    roots.extend(target.runtime_paths.clone());
    let closure = nix::path_info(&roots)?;
    let closure_paths = nix::closure_paths(&closure)?;
    let executable =
        fs::canonicalize(&target.entrypoint[0]).context("entrypoint does not exist")?;
    ensure!(
        closure_paths
            .iter()
            .any(|root| executable.starts_with(root)),
        "entrypoint is outside the declared program/runtime closure"
    );
    let driver_files = driver_identity(&allocation.launch)?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let workspace = runtime::Workspace::create(&state_dir.join(&run_id))?;
    let started = now();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut exit_code = None;
    let result = (|| -> anyhow::Result<()> {
        if let Some(image) = &image {
            allocation.launch = runtime::prepare_oci_launch(
                &job,
                &target,
                &allocation.launch,
                &workspace,
                caps.container_runtime.as_deref().unwrap(),
                image,
            )?;
        }
        for (name, input) in &job.artifacts.inputs {
            let destination = workspace.inputs.join(&input.path);
            cas::materialize_input(input, &destination)
                .with_context(|| format!("materializing input {name}"))?;
            inputs.push(json!({"name": name, "path": input.path, "sha256": cas::sha256_file(&destination)?}));
        }
        if cancellation.flag.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("cancelled before launch");
        }
        let code = match target.summary.executor.as_str() {
            "native" => runtime::run_native(
                &job,
                &target,
                &allocation.launch,
                &workspace,
                &cancellation.flag,
            )?,
            "oci" => runtime::run_container(
                &job,
                &target,
                &allocation.launch,
                &workspace,
                caps.container_runtime.as_deref().unwrap(),
                image.as_deref().unwrap(),
                &cancellation.flag,
            )?,
            _ => unreachable!(),
        };
        exit_code = Some(code);
        for (name, output) in &job.artifacts.outputs {
            let path = runtime::output_path(&workspace.outputs, &output.path);
            if !output.required && !workspace.outputs.join(&output.path).try_exists()? {
                continue;
            }
            let path =
                path.with_context(|| format!("required output {name} missing or invalid"))?;
            let digest = cas::sha256_file(&path)?;
            let uri = output
                .destination
                .as_ref()
                .map(|destination| cas::upload_output(&path, destination, &digest))
                .transpose()?;
            outputs.push(json!({"name": name, "path": output.path, "sha256": digest, "uri": uri}));
        }
        ensure!(code == 0, "workload exited with code {code}");
        Ok(())
    })();
    if let Err(error) = &result {
        if error.downcast_ref::<runtime::CleanupFailure>().is_some() {
            accelerator::quarantine(&format!("{error:#}"))?;
        }
    }
    let job_identity = json!({"source": source, "job": job.common_identity()});
    let job_id = canonical::sha256_value(&job_identity)?;
    let target_identity = json!({"job_id": job_id, "target": target, "closure": closure});
    let status = match exit_code {
        Some(124) => "timed_out",
        Some(130) => "cancelled",
        _ if result.is_err() && cancellation.flag.load(std::sync::atomic::Ordering::Relaxed) => {
            "cancelled"
        }
        _ if result.is_ok() => "succeeded",
        _ => "failed",
    };
    let payload = json!({
        "schema_version": 2, "canonicalization": "nix-compute-sorted-json-v1",
        "run_id": run_id, "job_id": job_id, "job_identity": job_identity,
        "target_id": canonical::sha256_value(&target_identity)?, "target_identity": target_identity,
        "source_ref": raw, "host": caps, "allocation": allocation.launch, "host_driver_files": driver_files,
        "closure_sha256": canonical::sha256_value(&closure)?, "image_archive": archive, "image_id": image,
        "inputs": inputs, "outputs": outputs, "status": status, "exit_code": exit_code,
        "error": result.as_ref().err().map(|e| format!("{e:#}")), "started_at": started, "finished_at": now()
    });
    let envelope = signer.sign(payload)?;
    let path = workspace.root.join("attestation.json");
    fs::write(&path, serde_json::to_vec_pretty(&envelope)?)?;
    println!("attestation: {}", path.display());
    result
}

fn driver_identity(launch: &accelerator::Launch) -> anyhow::Result<Value> {
    let mut files = BTreeMap::new();
    for device in &launch.devices {
        for root in device.driver_mounts.keys() {
            hash_tree(Path::new(root), &mut files)?;
        }
    }
    Ok(serde_json::to_value(files)?)
}

fn hash_tree(path: &Path, files: &mut BTreeMap<String, Value>) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        let resolved = path.canonicalize()?;
        ensure!(
            resolved.is_file(),
            "host driver directory symlink cannot be attested: {}",
            path.display()
        );
        files.insert(
            path.display().to_string(),
            json!({"target": resolved, "sha256": cas::sha256_file(path)?}),
        );
    } else if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            hash_tree(&entry?.path(), files)?;
        }
    } else if metadata.is_file() {
        files.insert(
            path.display().to_string(),
            json!({"sha256": cas::sha256_file(path)?}),
        );
    } else {
        anyhow::bail!("unsupported host driver file {}", path.display());
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
