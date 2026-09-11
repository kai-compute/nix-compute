mod accelerator;
mod attestation;
mod cas;
mod group;
mod process;
mod rendezvous;
mod runtime;

use anyhow::{ensure, Context};
use clap::{Parser, Subcommand};
use nix_compute_protocol::{canonical, model, store as nix};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Parser)]
#[command(
    name = "nix-compute-provider-reference",
    version,
    about = "Execute prebuilt Nix Compute tasks as a reference provider"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Run {
        task: PathBuf,
        #[arg(long)]
        context: Option<PathBuf>,
        #[arg(long)]
        coordination_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 60)]
        coordination_timeout: u64,
        #[arg(long)]
        signing_key: PathBuf,
        #[arg(long, default_value = ".nix-compute/runs")]
        state_dir: PathBuf,
    },
    Capabilities {
        task: Option<PathBuf>,
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
        Command::Run {
            task,
            context,
            coordination_dir,
            coordination_timeout,
            signing_key,
            state_dir,
        } => run(
            &task,
            context.as_deref(),
            coordination_dir.as_deref(),
            coordination_timeout,
            &signing_key,
            &state_dir,
        )?,
        Command::Capabilities { task } => {
            let caps = runtime::detect_capabilities()?;
            let value = if let Some(path) = task {
                let task = model::Task::load(&path)?;
                runtime::check_host(&task.target.summary, &caps)?;
                let inventory = accelerator::adapter(&task.target.summary.accelerator.id)?
                    .probe(&task.target)?;
                json!({"host": caps, "inventory": inventory})
            } else {
                serde_json::to_value(caps)?
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
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

fn run(
    path: &Path,
    context_path: Option<&Path>,
    coordination_dir: Option<&Path>,
    coordination_timeout: u64,
    signing_key: &Path,
    state_dir: &Path,
) -> anyhow::Result<()> {
    let signer = attestation::SigningIdentity::load(signing_key)?;
    let cancellation = process::Cancellation::install()?;
    let root = path.canonicalize()?;
    model::store_path(root.to_str().context("invalid task path")?)?;
    ensure!(
        root.parent() == Some(Path::new("/nix/store")),
        "task must be a top-level Nix store output"
    );
    let task = model::Task::load(&root)?;
    let job = &task.job;
    let target = &task.target;
    let mut rendezvous = None;
    let context: model::NodeContext = if let Some(path) = context_path {
        serde_json::from_slice(&fs::read(path)?)?
    } else {
        ensure!(
            target.summary.resources.nodes == 1,
            "multi-node execution requires --context and --coordination-dir"
        );
        let reservation =
            rendezvous::Reservation::allocate(&accelerator::lock_root().join("ports"))?;
        let port = reservation.port();
        rendezvous = Some(reservation);
        model::NodeContext {
            run_id: uuid::Uuid::new_v4().to_string(),
            node_id: "local".into(),
            node_rank: 0,
            node_count: 1,
            master_addr: "127.0.0.1".into(),
            master_port: port,
        }
    };
    context.validate(&target.summary)?;
    let closure = nix::path_info(&[root.display().to_string()])?;
    let closure_paths = nix::closure_paths(&closure)?;
    for path in std::iter::once(&target.entrypoint[0])
        .chain(std::iter::once(&target.program))
        .chain(target.probe.iter())
        .chain(target.runtime_paths.iter())
        .chain(job.artifacts.inputs.values().map(|i| &i.source))
        .chain(task.image.iter())
        .chain(std::iter::once(&task.source))
    {
        let resolved =
            fs::canonicalize(path).with_context(|| format!("missing task dependency {path}"))?;
        ensure!(
            closure_paths.iter().any(|root| resolved.starts_with(root)),
            "task dependency is outside bundle closure: {path}"
        );
    }
    let nar_hash = closure
        .get(root.to_str().unwrap())
        .and_then(|info| info["narHash"].as_str())
        .context("bundle NAR hash missing")?;
    let task_identity = json!({"path": root, "nar_hash": nar_hash});
    let task_id = canonical::sha256_value(&task_identity)?;
    let mut group = group::Group::join(
        coordination_dir,
        &context,
        &task_id,
        coordination_timeout,
        cancellation.flag.clone(),
    )?;
    let workspace = runtime::Workspace::create(
        &state_dir
            .join(&context.run_id)
            .join(format!("node-{}", context.node_rank)),
    )?;
    let started = now();
    let mut caps = None;
    let mut allocation = None;
    let mut archive = None;
    let mut image = None;
    let mut driver_files = Value::Null;
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut exit_code = None;
    let result = (|| -> anyhow::Result<()> {
        let host = runtime::detect_capabilities()?;
        runtime::check_host(&target.summary, &host)?;
        caps = Some(host);
        let inventory = accelerator::adapter(&target.summary.accelerator.id)?.probe(target)?;
        allocation = Some(accelerator::allocate(
            target,
            &inventory,
            &accelerator::lock_root(),
        )?);
        let allocation = allocation.as_mut().unwrap();
        driver_files = driver_identity(&allocation.launch)?;
        if let Some(path) = &task.image {
            image = Some(runtime::load_image(
                Path::new(path),
                caps.as_ref().unwrap().container_runtime.as_deref().unwrap(),
                job,
            )?);
            archive = Some(json!({"path": path, "sha256": cas::sha256_file(Path::new(path))?}));
        }
        for (name, input) in &job.artifacts.inputs {
            cas::materialize_input(input, &workspace.inputs.join(&input.path), &closure_paths)?;
            inputs.push(json!({"name": name, "path": input.path, "source": input.source}));
        }
        if let Some(image) = &image {
            allocation.launch = runtime::prepare_oci_launch(
                job,
                target,
                &allocation.launch,
                &workspace,
                caps.as_ref().unwrap().container_runtime.as_deref().unwrap(),
                image,
            )?;
        }
        allocation.launch.env.extend(context.environment());
        group.ready()?;
        ensure!(
            !cancellation.flag.load(std::sync::atomic::Ordering::Relaxed),
            "cancelled before launch"
        );
        if let Some(reservation) = &mut rendezvous {
            reservation.handoff();
        }
        let code = match target.summary.executor.as_str() {
            "native" => runtime::run_native(
                job,
                target,
                &allocation.launch,
                &workspace,
                &cancellation.flag,
            )?,
            "oci" => runtime::run_container(
                job,
                target,
                &allocation.launch,
                &workspace,
                caps.as_ref().unwrap().container_runtime.as_deref().unwrap(),
                image.as_deref().unwrap(),
                &cancellation.flag,
            )?,
            _ => unreachable!(),
        };
        exit_code = Some(code);
        if code != 0 {
            group.fail(format!("workload exited with code {code}"));
        }
        for (name, output) in &job.artifacts.outputs {
            if output.scope == "leader" && context.node_rank != 0 {
                continue;
            }
            if !output.required && !workspace.outputs.join(&output.path).try_exists()? {
                continue;
            }
            outputs.push(
                cas::collect_output(name, output, &workspace.outputs, context.node_rank)
                    .with_context(|| format!("required output {name} missing or invalid"))?,
            );
        }
        ensure!(code == 0, "workload exited with code {code}");
        Ok(())
    })();
    if let Err(error) = &result {
        if error.downcast_ref::<runtime::CleanupFailure>().is_some() {
            accelerator::quarantine(&format!("{error:#}"))?;
        }
    }
    let cancelled_before_finish = cancellation.flag.load(std::sync::atomic::Ordering::Relaxed);
    if let Err(error) = &result {
        group.fail(format!("{error:#}"));
    }
    let job_identity = json!({"source": task.source, "job": job.common_identity()});
    let job_id = canonical::sha256_value(&job_identity)?;
    let target_identity = json!({"job_id": job_id, "target": target, "closure": closure});
    let status = match exit_code {
        Some(124) => "timed_out",
        Some(130) => "cancelled",
        _ if result.is_ok() => "succeeded",
        None if cancelled_before_finish => "cancelled",
        _ => "failed",
    };
    let mut payload = json!({
        "schema_version": 3, "canonicalization": "nix-compute-sorted-json-v1",
        "run_id": context.run_id, "node": context, "task_id": task_id, "task_identity": task_identity,
        "job_id": job_id, "job_identity": job_identity,
        "target_id": canonical::sha256_value(&target_identity)?, "target_identity": target_identity,
        "host": caps, "allocation": allocation.as_ref().map(|a| &a.launch), "host_driver_files": driver_files,
        "closure_sha256": canonical::sha256_value(&closure)?, "image_archive": archive, "image_id": image,
        "inputs": inputs, "outputs": outputs, "status": status, "exit_code": exit_code,
        "error": result.as_ref().err().map(|e| format!("{e:#}")), "started_at": started, "finished_at": now()
    });
    let report = workspace.root.join("attestation.json");
    let paths = group.reports(&report)?;
    // Persist a provisional report before the barrier; never sign early group success.
    if status == "succeeded" && context.node_count > 1 {
        payload["status"] = "prepared".into();
    }
    let persistence = persist_report(&paths.provisional, &signer, &payload);
    let mut result = group.prepare(persistence.and(result));
    if status == "succeeded" {
        payload["status"] = if result.is_ok() {
            "succeeded"
        } else {
            "failed"
        }
        .into();
        payload["error"] = result.as_ref().err().map(|e| format!("{e:#}")).into();
        payload["finished_at"] = now().into();
        let destination = if result.is_ok() {
            &paths.candidate
        } else {
            &paths.provisional
        };
        result = persist_report(destination, &signer, &payload).and(result);
    }
    let result = group.finish(result);
    if let Err(error) = &result {
        if status == "succeeded" {
            payload["status"] = "failed".into();
            payload["error"] = format!("{error:#}").into();
            payload["finished_at"] = now().into();
            persist_report(&paths.provisional, &signer, &payload)?;
        }
    }
    println!("attestation: {}", report.display());
    result
}

fn persist_report(
    report: &Path,
    signer: &attestation::SigningIdentity,
    payload: &Value,
) -> anyhow::Result<()> {
    let parent = report.parent().context("missing report directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(&signer.sign(payload.clone())?)?)?;
    file.as_file().sync_all()?;
    file.persist(report).context("persisting signed report")?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
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
