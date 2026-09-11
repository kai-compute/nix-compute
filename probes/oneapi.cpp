#include <level_zero/ze_api.h>
#include <level_zero/zes_api.h>
#include <nlohmann/json.hpp>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <sstream>
#include <stdexcept>
#include <vector>

using nlohmann::json;

static void check(ze_result_t result, const char *operation) {
    if (result != ZE_RESULT_SUCCESS)
        throw std::runtime_error(std::string(operation) + " failed: " + std::to_string(result));
}

int main() {
    try {
        // Legacy Sysman initialization permits querying the same core device handles.
        setenv("ZES_ENABLE_SYSMAN", "1", 1);
        setenv("ZE_FLAT_DEVICE_HIERARCHY", "COMPOSITE", 1);
        check(zeInit(ZE_INIT_FLAG_GPU_ONLY), "zeInit");
        uint32_t count = 0;
        check(zeDriverGet(&count, nullptr), "zeDriverGet");
        if (count != 1) throw std::runtime_error("expected one Level Zero GPU driver");
        ze_driver_handle_t driver;
        check(zeDriverGet(&count, &driver), "zeDriverGet");
        ze_api_version_t version;
        check(zeDriverGetApiVersion(driver, &version), "zeDriverGetApiVersion");
        std::string runtime = std::to_string(ZE_MAJOR_VERSION(version)) + "." +
                              std::to_string(ZE_MINOR_VERSION(version)) + ".0";
        count = 0;
        check(zeDeviceGet(driver, &count, nullptr), "zeDeviceGet");
        std::vector<ze_device_handle_t> devices(count);
        check(zeDeviceGet(driver, &count, devices.data()), "zeDeviceGet");
        json inventory = {{"version", 1}, {"backend", "oneapi"}, {"devices", json::array()}};
        for (uint32_t index = 0; index < count; ++index) {
            auto handle = devices[index];
            ze_device_properties_t properties{};
            properties.stype = ZE_STRUCTURE_TYPE_DEVICE_PROPERTIES;
            check(zeDeviceGetProperties(handle, &properties), "zeDeviceGetProperties");
            if (properties.vendorId != 0x8086 || properties.type != ZE_DEVICE_TYPE_GPU)
                throw std::runtime_error("unexpected non-Intel GPU");
            ze_pci_ext_properties_t pci{};
            pci.stype = ZE_STRUCTURE_TYPE_PCI_EXT_PROPERTIES;
            check(zeDevicePciGetPropertiesExt(handle, &pci), "zeDevicePciGetPropertiesExt");
            std::ostringstream bdf;
            bdf << std::hex << std::setfill('0') << std::setw(4) << pci.address.domain << ":"
                << std::setw(2) << pci.address.bus << ":" << std::setw(2) << pci.address.device
                << "." << pci.address.function;
            std::vector<std::string> nodes;
            for (const auto &entry : std::filesystem::directory_iterator("/sys/class/drm")) {
                auto name = entry.path().filename().string();
                if (name.rfind("renderD", 0) != 0) continue;
                if (std::filesystem::canonical(entry.path() / "device").filename() == bdf.str())
                    nodes.push_back("/dev/dri/" + name);
            }
            if (nodes.size() != 1 || !std::filesystem::is_character_file(nodes[0]))
                throw std::runtime_error("cannot map Level Zero PCI identity to a render node");
            uint32_t memory_count = 0;
            check(zeDeviceGetMemoryProperties(handle, &memory_count, nullptr), "zeDeviceGetMemoryProperties");
            std::vector<ze_device_memory_properties_t> memory(memory_count);
            for (auto &item : memory) item.stype = ZE_STRUCTURE_TYPE_DEVICE_MEMORY_PROPERTIES;
            check(zeDeviceGetMemoryProperties(handle, &memory_count, memory.data()), "zeDeviceGetMemoryProperties");
            uint64_t bytes = 0;
            for (const auto &item : memory) bytes += item.totalSize;
            zes_device_properties_t management{};
            management.stype = ZES_STRUCTURE_TYPE_DEVICE_PROPERTIES;
            management.core.stype = ZE_STRUCTURE_TYPE_DEVICE_PROPERTIES;
            check(zesDeviceGetProperties(reinterpret_cast<zes_device_handle_t>(handle), &management), "zesDeviceGetProperties");
            inventory["devices"].push_back({
                {"id", bdf.str()}, {"index", index}, {"architecture", properties.name},
                {"memory_mib", bytes / (1024 * 1024)}, {"driver_version", management.driverVersion},
                {"runtime_version", runtime}, {"topology", {{"pci_bus_id", bdf.str()}}},
                {"device_nodes", nodes}, {"driver_mounts", json::object()}
            });
        }
        std::cout << inventory.dump() << '\n';
    } catch (const std::exception &error) {
        std::cerr << "oneapi probe failed: " << error.what() << '\n';
        return 1;
    }
}
