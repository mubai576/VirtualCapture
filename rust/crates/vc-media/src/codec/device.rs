//! 读取 PyroWave 已选中的 Vulkan 设备身份。
//!
//! 只查询 PyroWave 创建的同一个 `VkInstance`/`VkPhysicalDevice`，不另建探测设备、
//! 不改变设备选择。查询失败时明确返回 `status=unavailable`，不伪造 GPU 身份。

use std::ffi::{c_char, c_void};
use std::path::Path;
use std::ptr;

use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library};
use serde_json::{Value, json};

const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2: u32 = 1_000_059_001;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES: u32 = 1_000_071_004;
const VK_MAX_PHYSICAL_DEVICE_NAME_SIZE: usize = 256;
const VK_UUID_SIZE: usize = 16;
const VK_LUID_SIZE: usize = 8;
// Vulkan 1.0 VkPhysicalDeviceProperties 在 x64 上约 816 字节；留足余量，
// 同时只把这块缓冲区交给驱动写入，读取前缀字段。
const PROPERTIES_BUFFER: usize = 1024;

type VkInstance = *mut c_void;
type VkPhysicalDevice = *mut c_void;
type GetInstanceProcAddr = unsafe extern "system" fn(VkInstance, *const c_char) -> *mut c_void;
type GetPhysicalDeviceProperties = unsafe extern "system" fn(VkPhysicalDevice, *mut u8);
type GetPhysicalDeviceProperties2 =
    unsafe extern "system" fn(VkPhysicalDevice, *mut PhysicalDeviceProperties2);

#[repr(C)]
struct PhysicalDeviceIdProperties {
    s_type: u32,
    p_next: *mut c_void,
    device_uuid: [u8; VK_UUID_SIZE],
    driver_uuid: [u8; VK_UUID_SIZE],
    device_luid: [u8; VK_LUID_SIZE],
    device_node_mask: u32,
    device_luid_valid: u32,
}

#[repr(C)]
struct PhysicalDeviceProperties2 {
    s_type: u32,
    p_next: *mut PhysicalDeviceIdProperties,
    properties: [u8; PROPERTIES_BUFFER],
}

#[repr(C, align(8))]
struct PropertiesBuffer([u8; PROPERTIES_BUFFER]);

fn unavailable() -> Value {
    json!({"status": "unavailable"})
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("VkPhysicalDeviceProperties 固定偏移"),
    )
}

fn read_name(bytes: &[u8]) -> String {
    let raw = &bytes[20..20 + VK_MAX_PHYSICAL_DEVICE_NAME_SIZE];
    let end = raw.iter().position(|&byte| byte == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

fn device_type_name(kind: u32) -> &'static str {
    match kind {
        1 => "integrated_gpu",
        2 => "discrete_gpu",
        3 => "virtual_gpu",
        4 => "cpu",
        _ => "other",
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn selected_properties(properties: &[u8], uuid: Option<String>, luid: Option<String>) -> Value {
    json!({
        "status": "selected",
        "name": read_name(properties),
        "type": device_type_name(read_u32(properties, 16)),
        "vendor_id": read_u32(properties, 8),
        "device_id": read_u32(properties, 12),
        "api_version": read_u32(properties, 0),
        "driver_version": read_u32(properties, 4),
        "uuid": uuid,
        "luid": luid,
    })
}

/// 查询 PyroWave 选中的设备；任何一步失败都只返回 `status=unavailable`。
pub(super) fn settings(instance: VkInstance, physical: VkPhysicalDevice) -> Value {
    if instance.is_null() || physical.is_null() {
        return unavailable();
    }
    // SAFETY: 只从系统目录加载 Vulkan loader。
    let Ok(loader) = (unsafe {
        Library::load_with_flags(Path::new("vulkan-1.dll"), LOAD_LIBRARY_SEARCH_SYSTEM32)
    }) else {
        return unavailable();
    };
    let Ok(get_instance_proc_addr) =
        (unsafe { loader.get::<GetInstanceProcAddr>(b"vkGetInstanceProcAddr\0") })
    else {
        return unavailable();
    };
    let get_instance_proc_addr = *get_instance_proc_addr;

    let properties2_name = b"vkGetPhysicalDeviceProperties2\0";
    let properties2 =
        unsafe { get_instance_proc_addr(instance, properties2_name.as_ptr().cast::<c_char>()) };
    if !properties2.is_null() {
        let mut ids = PhysicalDeviceIdProperties {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES,
            p_next: ptr::null_mut(),
            device_uuid: [0; VK_UUID_SIZE],
            driver_uuid: [0; VK_UUID_SIZE],
            device_luid: [0; VK_LUID_SIZE],
            device_node_mask: 0,
            device_luid_valid: 0,
        };
        let mut properties = PhysicalDeviceProperties2 {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
            p_next: &mut ids,
            properties: [0; PROPERTIES_BUFFER],
        };
        // SAFETY: 函数指针由 Vulkan loader 返回，参数布局与 vulkan_core.h 一致。
        let function: GetPhysicalDeviceProperties2 = unsafe { std::mem::transmute(properties2) };
        unsafe { function(physical, &mut properties) };
        let uuid = Some(hex(&ids.device_uuid));
        let luid = if ids.device_luid_valid != 0 {
            Some(hex(&ids.device_luid))
        } else {
            None
        };
        return selected_properties(&properties.properties, uuid, luid);
    }

    // Vulkan 1.0 回退：只有基础属性，不伪造 deviceUUID/LUID。
    let properties_name = b"vkGetPhysicalDeviceProperties\0";
    let properties =
        unsafe { get_instance_proc_addr(instance, properties_name.as_ptr().cast::<c_char>()) };
    if properties.is_null() {
        return unavailable();
    }
    let mut buffer = PropertiesBuffer([0; PROPERTIES_BUFFER]);
    // SAFETY: 同上；缓冲区大于 VkPhysicalDeviceProperties。
    let function: GetPhysicalDeviceProperties = unsafe { std::mem::transmute(properties) };
    unsafe { function(physical, buffer.0.as_mut_ptr()) };
    selected_properties(&buffer.0, None, None)
}
