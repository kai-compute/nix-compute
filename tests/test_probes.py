import sys
import unittest
from types import SimpleNamespace as NS
from unittest.mock import Mock, patch

from probes import inventory


class ProbeTests(unittest.TestCase):
    def test_cuda_preserves_nvml_uuid_and_rejects_mig(self):
        nvml = NS(
            nvmlInit=Mock(), nvmlShutdown=Mock(), nvmlDeviceGetCount=lambda: 1,
            nvmlDeviceGetHandleByIndex=lambda index: "handle",
            nvmlDeviceGetMigMode=Mock(return_value=(0, 0)),
            NVMLError_NotSupported=type("NotSupported", (Exception,), {}),
            nvmlDeviceGetCudaComputeCapability=lambda handle: (9, 0),
            nvmlDeviceGetPciInfo=lambda handle: NS(busId=b"0000:41:00.0"),
            nvmlDeviceGetUUID=lambda handle: b"GPU-stable",
            nvmlDeviceGetMemoryInfo=lambda handle: NS(total=80 * 2**30),
            nvmlSystemGetDriverVersion=lambda: b"550.54.15",
        )
        torch = NS(cuda=NS(init=Mock()), version=NS(cuda="12.4"))
        with patch.dict(sys.modules, {"pynvml": nvml, "torch": torch}):
            result = inventory.cuda()[0]
            self.assertEqual(result["id"], "GPU-stable")
            self.assertEqual(result["architecture"], "sm_90")
            self.assertEqual(result["memory_mib"], 81920)
            nvml.nvmlDeviceGetMigMode.return_value = (1, 1)
            with self.assertRaisesRegex(RuntimeError, "MIG"):
                inventory.cuda()

    def test_rocm_joins_smi_and_hip_by_pci_identity(self):
        buses = ["0000:41:00.0", "0000:42:00.0"]
        smi = NS(
            amdsmi_init=Mock(), amdsmi_shut_down=Mock(),
            amdsmi_get_processor_handles=lambda: list(reversed(buses)),
            amdsmi_get_gpu_device_bdf=lambda handle: handle,
            amdsmi_get_gpu_driver_info=lambda handle: {"driver_version": "6.8.0" if handle == buses[0] else "6.9.0"},
        )
        torch = NS(version=NS(hip="6.3.0"), cuda=NS(
            init=Mock(), device_count=lambda: 2,
            get_device_properties=lambda index: NS(gcnArchName="gfx942", total_memory=192 * 2**30),
        ))

        def pci(buffer, size, index):
            buffer.value = buses[index].encode()
            return 0

        hip = NS(hipDeviceGetPCIBusId=Mock(side_effect=pci))
        with patch.dict(sys.modules, {"amdsmi": smi, "torch": torch}), \
                patch.object(inventory.ctypes, "CDLL", return_value=hip), \
                patch.object(inventory.ctypes.util, "find_library", return_value="libamdhip64.so"), \
                patch.object(inventory, "render_node", side_effect=lambda bus, vendor: "/dev/dri/renderD" + str(128 + buses.index(bus))):
            result = inventory.rocm()
            self.assertEqual(result[0]["driver_version"], "6.8.0")
            self.assertEqual(result[1]["device_nodes"], ["/dev/kfd", "/dev/dri/renderD129"])
            self.assertEqual(result[1]["id"], buses[1])

    def test_tpu_records_local_topology_and_rejects_unknown_memory(self):
        item = NS(id=4, device_kind="TPU v5 lite", coords=(1, 0, 0), core_on_chip=0,
                  process_index=0, memory_stats=Mock(return_value={"bytes_limit": 16 * 2**30}))
        jax = NS(local_devices=Mock(return_value=[item]))
        with patch.dict(sys.modules, {"jax": jax, "jaxlib": NS(__version__="0.4.35")}), \
                patch.object(inventory.importlib.metadata, "version", return_value="0.0.10"):
            result = inventory.tpu()[0]
            self.assertEqual(result["topology"]["coords"], "1,0,0")
            self.assertEqual(result["id"], "tpu-4")
            jax.local_devices.assert_called_with(backend="tpu")
            item.memory_stats.return_value = None
            with self.assertRaisesRegex(RuntimeError, "memory"):
                inventory.tpu()

    def test_metal_bounds_gpu_memory_by_recommended_working_set(self):
        item = NS(hasUnifiedMemory=Mock(return_value=True), registryID=lambda: 1000,
                  name=lambda: "Apple M3", recommendedMaxWorkingSetSize=lambda: 24 * 2**30)
        with patch.dict(sys.modules, {"Metal": NS(MTLCreateSystemDefaultDevice=lambda: item)}), \
                patch.object(inventory.subprocess, "check_output", side_effect=["15.1\n", str(32 * 2**30)]):
            result = inventory.metal()[0]
            self.assertEqual(result["memory_mib"], 24576)
            self.assertEqual(result["topology"], {"unified_memory": "true"})
            item.hasUnifiedMemory.return_value = False
            with self.assertRaisesRegex(RuntimeError, "unified-memory"):
                inventory.metal()

    def test_cann_maps_logical_index_to_physical_device_node(self):
        acl = NS(get_version=lambda: (8, 0, 0, 0), rt=NS(get_device_phy_id_by_index=lambda index: (3, 0)))
        npu = NS(init=Mock(), device_count=lambda: 1,
                 get_device_properties=lambda index: NS(name="Ascend910B", total_memory=64 * 2**30))
        with patch.dict(sys.modules, {"acl": acl, "torch": NS(), "torch_npu": NS(npu=npu)}), \
                patch.object(inventory.pathlib.Path, "read_text", return_value="Version=24.1.0\n"), \
                patch.object(inventory.pathlib.Path, "exists", return_value=True):
            result = inventory.cann()[0]
            self.assertEqual(result["index"], 0)
            self.assertEqual(result["id"], "ascend-3")
            self.assertIn("/dev/davinci3", result["device_nodes"])
            self.assertNotIn("/dev/davinci0", result["device_nodes"])


if __name__ == "__main__":
    unittest.main()
