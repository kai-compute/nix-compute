use crate::{
    canonical::sha256_bytes,
    model::{Accelerator, Target},
    process,
};
use anyhow::{ensure, Context};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub id: String,
    pub index: u32,
    pub architecture: String,
    pub memory_mib: u64,
    pub driver_version: String,
    pub runtime_version: String,
    pub topology: BTreeMap<String, String>,
    pub device_nodes: Vec<String>,
    pub driver_mounts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub version: u32,
    pub backend: String,
    pub devices: Vec<Device>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Launch {
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub devices: Vec<Device>,
}

pub struct Allocation {
    pub launch: Launch,
    _locks: Vec<File>,
}

pub trait Adapter: Sync {
    fn id(&self) -> &'static str;
    fn configure(&self, devices: &[Device], inventory: &Inventory) -> anyhow::Result<Launch>;
    fn probe(&self, target: &Target) -> anyhow::Result<Inventory> {
        if self.id() == "cpu" {
            return Ok(Inventory {
                version: 1,
                backend: "cpu".into(),
                devices: vec![],
            });
        }
        let executable = target
            .probe
            .as_deref()
            .context("missing locked accelerator probe")?;
        let mut command = Command::new(executable);
        // Probe wrappers must establish their own locked SDK/library environment.
        command.env_clear().env("PATH", "/usr/bin:/bin");
        let output = process::capture(&mut command, 30)?;
        parse_inventory(&output, self.id())
    }
}

struct Cpu;
struct Cuda;
struct Rocm;
struct Tpu;
struct Metal;
struct Cann;
struct Oneapi;

static ADAPTERS: [&'static dyn Adapter; 7] = [&Cpu, &Cuda, &Rocm, &Tpu, &Metal, &Cann, &Oneapi];

pub fn adapter(id: &str) -> anyhow::Result<&'static dyn Adapter> {
    ADAPTERS
        .iter()
        .copied()
        .find(|a| a.id() == id)
        .with_context(|| format!("unknown accelerator {id}"))
}

pub fn parse_inventory(bytes: &[u8], backend: &str) -> anyhow::Result<Inventory> {
    let inventory: Inventory =
        serde_json::from_slice(bytes).context("invalid accelerator inventory JSON")?;
    ensure!(
        inventory.version == 1 && inventory.backend == backend,
        "probe version/backend mismatch"
    );
    let mut ids = BTreeSet::new();
    let mut indexes = BTreeSet::new();
    for device in &inventory.devices {
        ensure!(
            !device.id.is_empty()
                && device
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c)),
            "invalid device ID"
        );
        ensure!(
            ids.insert(&device.id) && indexes.insert(device.index),
            "duplicate device identity"
        );
        ensure!(
            device.memory_mib > 0
                && !device.architecture.is_empty()
                && !device.runtime_version.is_empty(),
            "unknown device memory, architecture or runtime"
        );
        driver_version(&device.driver_version)?;
        for node in &device.device_nodes {
            ensure!(
                node.starts_with("/dev/")
                    && !node.contains([',', ':', '\n'])
                    && !Path::new(node)
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir)),
                "invalid device node"
            );
        }
        for (source, destination) in &device.driver_mounts {
            ensure!(
                Path::new(source).is_absolute()
                    && Path::new(destination).is_absolute()
                    && !source.contains([',', ':'])
                    && !destination.contains([',', ':']),
                "invalid host driver mount"
            );
        }
    }
    Ok(inventory)
}

fn driver_version(raw: &str) -> anyhow::Result<semver::Version> {
    // Vendor numeric versions may omit a patch or use leading zeros (535.104.05).
    let components = raw
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>();
    match components.as_deref() {
        Ok([major, minor]) => Ok(semver::Version::new(*major, *minor, 0)),
        Ok([major, minor, patch]) => Ok(semver::Version::new(*major, *minor, *patch)),
        _ => semver::Version::parse(raw).with_context(|| format!("unknown driver version {raw:?}")),
    }
}

pub fn matching(requirement: &Accelerator, inventory: &Inventory) -> anyhow::Result<Vec<Device>> {
    ensure!(
        requirement.id == inventory.backend,
        "accelerator inventory mismatch"
    );
    if requirement.id == "cpu" {
        return Ok(vec![]);
    }
    let range = semver::VersionReq::parse(
        requirement
            .driver_range
            .as_deref()
            .context("missing driver range")?,
    )?;
    let mut devices: Vec<_> = inventory
        .devices
        .iter()
        .filter(|d| {
            d.memory_mib >= requirement.min_memory_mib
                && requirement.architectures.contains(&d.architecture)
                && requirement.runtime_version.as_ref() == Some(&d.runtime_version)
                && driver_version(&d.driver_version).is_ok_and(|v| range.matches(&v))
                && requirement
                    .topology
                    .iter()
                    .all(|(key, value)| d.topology.get(key) == Some(value))
        })
        .cloned()
        .collect();
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    ensure!(devices.len() >= requirement.count as usize, "{} needs {} compatible devices; found {} (architecture, memory, driver, runtime and topology checked)", requirement.id, requirement.count, devices.len());
    Ok(devices)
}

pub fn allocate(
    target: &Target,
    inventory: &Inventory,
    lock_root: &Path,
) -> anyhow::Result<Allocation> {
    let backend = adapter(&target.summary.accelerator.id)?;
    let candidates = matching(&target.summary.accelerator, inventory)?;
    fs::create_dir_all(lock_root)?;
    ensure!(
        !lock_root.join("quarantine.json").try_exists()?,
        "node is quarantined after a container cleanup failure; inspect {}",
        lock_root.join("quarantine.json").display()
    );
    let mut locks = vec![];
    let mut devices = vec![];
    for device in candidates {
        let filename = sha256_bytes(format!("{}:{}", backend.id(), device.id).as_bytes());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_root.join(filename))?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                locks.push(file);
                devices.push(device);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error.into()),
        }
        if devices.len() == target.summary.accelerator.count as usize {
            break;
        }
    }
    ensure!(
        backend.id() == "cpu" || devices.len() == target.summary.accelerator.count as usize,
        "compatible devices are already allocated"
    );
    let launch = backend.configure(&devices, inventory)?;
    Ok(Allocation {
        launch,
        _locks: locks,
    })
}

pub fn lock_root() -> PathBuf {
    std::env::var_os("NIX_COMPUTE_DEVICE_LOCK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let uid = unsafe { libc::geteuid() };
            std::env::temp_dir().join(format!("nix-compute-devices-{uid}"))
        })
}

pub fn quarantine(reason: &str) -> anyhow::Result<()> {
    let root = lock_root();
    fs::create_dir_all(&root)?;
    fs::write(
        root.join("quarantine.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"reason": reason}))?,
    )?;
    Ok(())
}

fn indexes(devices: &[Device]) -> String {
    devices
        .iter()
        .map(|d| d.index.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
fn launch(devices: &[Device]) -> Launch {
    Launch {
        devices: devices.to_vec(),
        ..Launch::default()
    }
}

fn render_nodes(devices: &[Device], required: &str) -> anyhow::Result<Vec<String>> {
    let nodes: BTreeSet<_> = devices
        .iter()
        .flat_map(|d| d.device_nodes.iter())
        .cloned()
        .collect();
    ensure!(
        devices.iter().all(|d| d
            .device_nodes
            .iter()
            .any(|p| p.starts_with("/dev/dri/renderD"))),
        "selected devices lack mapped render nodes"
    );
    ensure!(
        nodes
            .iter()
            .all(|p| p.starts_with("/dev/dri/renderD") || p == required),
        "unexpected device node for backend"
    );
    Ok(nodes.into_iter().map(|n| format!("--device={n}")).collect())
}

impl Adapter for Cpu {
    fn id(&self) -> &'static str {
        "cpu"
    }
    fn configure(&self, _: &[Device], _: &Inventory) -> anyhow::Result<Launch> {
        Ok(Launch::default())
    }
}
impl Adapter for Cuda {
    fn id(&self) -> &'static str {
        "cuda"
    }
    fn configure(&self, devices: &[Device], _: &Inventory) -> anyhow::Result<Launch> {
        let mut result = launch(devices);
        for d in devices {
            ensure!(
                d.id.starts_with("GPU-") || d.id.starts_with("MIG-"),
                "CUDA requires a stable NVML UUID"
            );
            result
                .args
                .push(format!("--device=nvidia.com/gpu={}", d.id));
        }
        result.env.insert(
            "CUDA_VISIBLE_DEVICES".into(),
            devices
                .iter()
                .map(|d| d.id.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        Ok(result)
    }
}
impl Adapter for Rocm {
    fn id(&self) -> &'static str {
        "rocm"
    }
    fn configure(&self, devices: &[Device], _: &Inventory) -> anyhow::Result<Launch> {
        let mut result = launch(devices);
        ensure!(
            devices
                .iter()
                .all(|d| d.device_nodes.iter().any(|p| p == "/dev/kfd")),
            "ROCm requires KFD"
        );
        result.args = render_nodes(devices, "/dev/kfd")?;
        result
            .env
            .insert("ROCR_VISIBLE_DEVICES".into(), indexes(devices));
        Ok(result)
    }
}
impl Adapter for Tpu {
    fn id(&self) -> &'static str {
        "tpu"
    }
    fn configure(&self, devices: &[Device], inventory: &Inventory) -> anyhow::Result<Launch> {
        ensure!(
            devices.len() == inventory.devices.len(),
            "native TPU adapter requires allocation of the complete local topology"
        );
        let mut result = launch(devices);
        result.env.insert("JAX_PLATFORMS".into(), "tpu".into());
        result.env.insert("PJRT_DEVICE".into(), "TPU".into());
        Ok(result)
    }
}
impl Adapter for Metal {
    fn id(&self) -> &'static str {
        "metal"
    }
    fn configure(&self, devices: &[Device], inventory: &Inventory) -> anyhow::Result<Launch> {
        ensure!(
            devices.len() == 1 && inventory.devices.len() == 1,
            "Metal adapter requires one unified-memory device"
        );
        let mut result = launch(devices);
        result
            .env
            .insert("PYTORCH_ENABLE_MPS_FALLBACK".into(), "0".into());
        Ok(result)
    }
}
impl Adapter for Cann {
    fn id(&self) -> &'static str {
        "cann"
    }
    fn configure(&self, devices: &[Device], _: &Inventory) -> anyhow::Result<Launch> {
        let mut result = launch(devices);
        let mut nodes = BTreeSet::new();
        let mut mounts = BTreeMap::new();
        let mut physical_ids = Vec::new();
        for d in devices {
            let physical: u32 =
                d.id.strip_prefix("ascend-")
                    .context("Ascend requires a physical device ID")?
                    .parse()?;
            physical_ids.push(physical.to_string());
            ensure!(
                d.device_nodes.contains(&format!("/dev/davinci{physical}")),
                "Ascend device mapping missing"
            );
            ensure!(
                d.driver_mounts.contains_key("/usr/local/Ascend/driver"),
                "Ascend host driver integration missing"
            );
            for node in &d.device_nodes {
                ensure!(
                    node == &format!("/dev/davinci{physical}")
                        || ["/dev/davinci_manager", "/dev/devmm_svm", "/dev/hisi_hdc"]
                            .contains(&node.as_str()),
                    "unexpected Ascend device node"
                );
                nodes.insert(node.clone());
            }
            for (source, destination) in &d.driver_mounts {
                ensure!(
                    source == "/usr/local/Ascend/driver" && destination == source,
                    "unexpected Ascend driver mount"
                );
                mounts.insert(source.clone(), destination.clone());
            }
        }
        result
            .args
            .extend(nodes.into_iter().map(|n| format!("--device={n}")));
        result.args.extend(
            mounts
                .iter()
                .map(|(s, d)| format!("--mount=type=bind,src={s},dst={d},readonly")),
        );
        result
            .env
            .insert("ASCEND_RT_VISIBLE_DEVICES".into(), physical_ids.join(","));
        Ok(result)
    }
}
impl Adapter for Oneapi {
    fn id(&self) -> &'static str {
        "oneapi"
    }
    fn configure(&self, devices: &[Device], _: &Inventory) -> anyhow::Result<Launch> {
        let mut result = launch(devices);
        result.args = render_nodes(devices, "")?;
        result.env.insert(
            "ONEAPI_DEVICE_SELECTOR".into(),
            format!("level_zero:{}", indexes(devices)),
        );
        result
            .env
            .insert("ZE_FLAT_DEVICE_HIERARCHY".into(), "COMPOSITE".into());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vendor_driver_versions_are_normalized_before_semver_matching() {
        assert_eq!(
            driver_version("535.104.05").unwrap(),
            semver::Version::new(535, 104, 5)
        );
        assert_eq!(
            driver_version("15.1").unwrap(),
            semver::Version::new(15, 1, 0)
        );
        assert!(driver_version("unknown").is_err());
    }
    #[test]
    fn allocation_is_exclusive_and_released_on_drop() {
        let (_, mut target) = crate::model::fixture();
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/inventories.json")).unwrap();
        let inventory =
            parse_inventory(&serde_json::to_vec(&fixtures["cuda"]).unwrap(), "cuda").unwrap();
        target.summary.accelerator = Accelerator {
            id: "cuda".into(),
            count: 1,
            min_memory_mib: 1,
            architectures: vec![inventory.devices[0].architecture.clone()],
            driver_range: Some(">=0.0.0".into()),
            runtime_version: Some(inventory.devices[0].runtime_version.clone()),
            topology: BTreeMap::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let first = allocate(&target, &inventory, dir.path()).unwrap();
        assert!(allocate(&target, &inventory, dir.path()).is_err());
        drop(first);
        assert!(allocate(&target, &inventory, dir.path()).is_ok());
        fs::write(dir.path().join("quarantine.json"), "{}").unwrap();
        assert!(allocate(&target, &inventory, dir.path()).is_err());
    }
    #[test]
    fn six_backends_match_and_configure_fixture_devices() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/inventories.json")).unwrap();
        for backend in ["cuda", "rocm", "tpu", "metal", "cann", "oneapi"] {
            let inventory =
                parse_inventory(&serde_json::to_vec(&fixtures[backend]).unwrap(), backend).unwrap();
            let d = &inventory.devices[0];
            let mut req = Accelerator {
                id: backend.into(),
                count: 1,
                min_memory_mib: 1024,
                architectures: vec![d.architecture.clone()],
                driver_range: Some(">=0.0.0".into()),
                runtime_version: Some(d.runtime_version.clone()),
                topology: BTreeMap::new(),
            };
            let devices = matching(&req, &inventory).unwrap();
            let config = adapter(backend)
                .unwrap()
                .configure(&devices, &inventory)
                .unwrap();
            assert_eq!(config.devices[0].id, d.id);
            if ["cuda", "rocm", "cann", "oneapi"].contains(&backend) {
                assert!(config.args.iter().any(|a| a.starts_with("--device=")));
            }
            req.min_memory_mib = u64::MAX;
            assert!(matching(&req, &inventory).is_err());
            req.min_memory_mib = 1;
            req.runtime_version = Some("unknown".into());
            assert!(matching(&req, &inventory).is_err());
            req.runtime_version = Some(d.runtime_version.clone());
            req.architectures = vec!["unsupported".into()];
            assert!(matching(&req, &inventory).is_err());
            req.architectures = vec![d.architecture.clone()];
            req.driver_range = Some("<0.0.0".into());
            assert!(matching(&req, &inventory).is_err());
            req.driver_range = Some(">=0.0.0".into());
            req.topology.insert("fabric".into(), "missing".into());
            assert!(matching(&req, &inventory).is_err());
        }
    }
    #[test]
    fn probe_rejects_unknown_capabilities_and_duplicate_identity() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/inventories.json")).unwrap();
        let mut inventory = fixtures["cuda"].clone();
        inventory["devices"][0]["driver_version"] = "unknown".into();
        assert!(parse_inventory(&serde_json::to_vec(&inventory).unwrap(), "cuda").is_err());
        let mut inventory = fixtures["cuda"].clone();
        let duplicate = inventory["devices"][0].clone();
        inventory["devices"].as_array_mut().unwrap().push(duplicate);
        assert!(parse_inventory(&serde_json::to_vec(&inventory).unwrap(), "cuda").is_err());
    }
}
