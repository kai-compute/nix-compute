"""Vendor probes. Dependencies must come from the caller's locked Nix interpreter."""

import ctypes
import ctypes.util
import importlib.metadata
import json
import pathlib
import subprocess
import sys


def text(value):
    return value.decode() if isinstance(value, bytes) else str(value)


def device(identity, index, architecture, memory, driver, runtime, **extra):
    return dict(id=text(identity), index=index, architecture=text(architecture),
                memory_mib=int(memory), driver_version=text(driver),
                runtime_version=text(runtime), topology=extra.get("topology", {}),
                device_nodes=extra.get("nodes", []), driver_mounts=extra.get("mounts", {}))


def render_node(bdf, vendor):
    matches = []
    for node in pathlib.Path("/sys/class/drm").glob("renderD*"):
        pci = (node / "device").resolve()
        if pci.name.lower() == bdf.lower() and (pci / "vendor").read_text().strip() == vendor:
            matches.append("/dev/dri/" + node.name)
    if len(matches) != 1 or not pathlib.Path(matches[0]).exists():
        raise RuntimeError("cannot uniquely map PCI device to a render node: " + bdf)
    return matches[0]


def cuda():
    import pynvml as nvml
    import torch
    nvml.nvmlInit()
    torch.cuda.init()
    if not torch.version.cuda:
        raise RuntimeError("probe interpreter does not contain a CUDA runtime")
    result = []
    for index in range(nvml.nvmlDeviceGetCount()):
        handle = nvml.nvmlDeviceGetHandleByIndex(index)
        try:
            if nvml.nvmlDeviceGetMigMode(handle)[0]:
                raise RuntimeError("MIG partitions require a partition-aware custom probe")
        except nvml.NVMLError_NotSupported:
            pass
        major, minor = nvml.nvmlDeviceGetCudaComputeCapability(handle)
        pci = text(nvml.nvmlDeviceGetPciInfo(handle).busId)
        result.append(device(nvml.nvmlDeviceGetUUID(handle), index, f"sm_{major}{minor}",
                             nvml.nvmlDeviceGetMemoryInfo(handle).total // 2**20,
                             nvml.nvmlSystemGetDriverVersion(), torch.version.cuda,
                             topology={"pci_bus_id": pci}))
    nvml.nvmlShutdown()
    return result


def rocm():
    import amdsmi
    import torch
    if not torch.version.hip:
        raise RuntimeError("probe interpreter does not contain a ROCm runtime")
    torch.cuda.init()
    hip = ctypes.CDLL(ctypes.util.find_library("amdhip64") or "libamdhip64.so")
    hip.hipDeviceGetPCIBusId.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
    hip.hipDeviceGetPCIBusId.restype = ctypes.c_int
    amdsmi.amdsmi_init()
    handles = {text(amdsmi.amdsmi_get_gpu_device_bdf(h)).lower(): h
               for h in amdsmi.amdsmi_get_processor_handles()}
    result = []
    for index in range(torch.cuda.device_count()):
        bus = ctypes.create_string_buffer(32)
        if hip.hipDeviceGetPCIBusId(bus, len(bus), index) != 0:
            raise RuntimeError("HIP PCI query failed")
        bdf = bus.value.decode().lower()
        handle = handles[bdf]
        props = torch.cuda.get_device_properties(index)
        driver = amdsmi.amdsmi_get_gpu_driver_info(handle)["driver_version"]
        result.append(device(bdf, index, props.gcnArchName, props.total_memory // 2**20,
                             driver, torch.version.hip, topology={"pci_bus_id": bdf},
                             nodes=["/dev/kfd", render_node(bdf, "0x1002")]))
    amdsmi.amdsmi_shut_down()
    return result


def tpu():
    import jax
    import jaxlib
    driver = importlib.metadata.version("libtpu")
    result = []
    for index, item in enumerate(jax.local_devices(backend="tpu")):
        stats = item.memory_stats()
        if not stats or "bytes_limit" not in stats:
            raise RuntimeError("PJRT did not report TPU memory capacity")
        result.append(device(f"tpu-{item.id}", index, item.device_kind,
                             stats["bytes_limit"] // 2**20, driver, jaxlib.__version__,
                             topology={"coords": ",".join(map(str, item.coords)),
                                       "core_on_chip": str(item.core_on_chip),
                                       "process_index": str(item.process_index)}))
    return result


def metal():
    import Metal
    item = Metal.MTLCreateSystemDefaultDevice()
    if item is None or not item.hasUnifiedMemory():
        raise RuntimeError("no Apple unified-memory Metal device")
    version = subprocess.check_output(["/usr/bin/sw_vers", "-productVersion"], text=True).strip()
    memory = int(subprocess.check_output(["/usr/sbin/sysctl", "-n", "hw.memsize"], text=True))
    return [device(f"metal-{item.registryID()}", 0, item.name(),
                   min(memory, item.recommendedMaxWorkingSetSize()) // 2**20,
                   version, version, topology={"unified_memory": "true"})]


def cann():
    import acl
    import torch
    import torch_npu
    torch_npu.npu.init()
    info = pathlib.Path("/usr/local/Ascend/driver/version.info").read_text()
    versions = dict(line.split("=", 1) for line in info.splitlines() if "=" in line)
    driver = versions.get("Version") or versions.get("version")
    if not driver:
        raise RuntimeError("Ascend driver version is unknown")
    major, minor, patch, status = acl.get_version()
    if status != 0:
        raise RuntimeError("ACL version query failed")
    runtime = f"{major}.{minor}.{patch}"
    result = []
    for index in range(torch_npu.npu.device_count()):
        props = torch_npu.npu.get_device_properties(index)
        physical_id, status = acl.rt.get_device_phy_id_by_index(index)
        if status != 0:
            raise RuntimeError("ACL physical device mapping failed")
        nodes = [f"/dev/davinci{physical_id}", "/dev/davinci_manager", "/dev/devmm_svm", "/dev/hisi_hdc"]
        if not all(pathlib.Path(node).exists() for node in nodes):
            raise RuntimeError("Ascend driver device nodes are missing")
        result.append(device(f"ascend-{physical_id}", index, props.name, props.total_memory // 2**20,
                             driver, runtime, nodes=nodes,
                             mounts={"/usr/local/Ascend/driver": "/usr/local/Ascend/driver"}))
    return result


PROBES = {"cuda": cuda, "rocm": rocm, "tpu": tpu, "metal": metal, "cann": cann}

if __name__ == "__main__":
    backend = sys.argv[1]
    try:
        devices = PROBES[backend]()
        print(json.dumps({"version": 1, "backend": backend, "devices": devices}))
    except Exception as error:
        print(f"{backend} probe failed: {error}", file=sys.stderr)
        sys.exit(1)
