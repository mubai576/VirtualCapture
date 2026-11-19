//! 手写 COM 对象共用的原始 Win32/DirectShow ABI 定义。
//!
//! 参考实现为历史 C++ 滤镜 `vcam/vcam.cpp`：vtable、字段顺序与签名逐一对应。
//! 媒体类型与能力结构在此自行定义，而不使用 windows crate 的结构体（后者用
//! `ManuallyDrop` 包裹接口指针），以便所有权保持显式；所有布局都有断言校验。

#![allow(non_snake_case, non_upper_case_globals)]

use core::ffi::c_void;
use core::mem::size_of;

use windows::core::{GUID, HRESULT};

// ---------- 滤镜用到的 HRESULT 值 ----------

pub const S_OK: HRESULT = HRESULT(0);
pub const S_FALSE: HRESULT = HRESULT(1);
pub const E_NOTIMPL: HRESULT = HRESULT(0x8000_4001u32 as i32);
pub const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002u32 as i32);
pub const E_POINTER: HRESULT = HRESULT(0x8000_4003u32 as i32);
pub const E_FAIL: HRESULT = HRESULT(0x8000_4005u32 as i32);
pub const E_UNEXPECTED: HRESULT = HRESULT(0x8000_FFFFu32 as i32);
pub const E_OUTOFMEMORY: HRESULT = HRESULT(0x8007_000Eu32 as i32);
pub const E_INVALIDARG: HRESULT = HRESULT(0x8007_0057u32 as i32);
pub const E_PROP_ID_UNSUPPORTED: HRESULT = HRESULT(0x8007_0490u32 as i32);
pub const CLASS_E_NOAGGREGATION: HRESULT = HRESULT(0x8004_0110u32 as i32);
pub const CLASS_E_CLASSNOTAVAILABLE: HRESULT = HRESULT(0x8004_0111u32 as i32);
/// olectl.h 中 `MAKE_HRESULT(SEVERITY_ERROR, FACILITY_ITF, 0x0201)` 的值。
pub const SELFREG_E_CLASS: HRESULT = HRESULT(0x8004_0201u32 as i32);
pub const VFW_E_ALREADY_CONNECTED: HRESULT = HRESULT(0x8004_0204u32 as i32);
pub const VFW_E_INVALID_DIRECTION: HRESULT = HRESULT(0x8004_0208u32 as i32);
pub const VFW_E_NOT_CONNECTED: HRESULT = HRESULT(0x8004_0209u32 as i32);
pub const VFW_E_SIZENOTSET: HRESULT = HRESULT(0x8004_0212u32 as i32);
pub const VFW_E_NOT_FOUND: HRESULT = HRESULT(0x8004_0216u32 as i32);
pub const VFW_E_TYPE_NOT_ACCEPTED: HRESULT = HRESULT(0x8004_022Au32 as i32);
pub const VFW_E_NO_TRANSPORT: HRESULT = HRESULT(0x8004_0266u32 as i32);
pub const VFW_S_CANT_CUE: HRESULT = HRESULT(0x0004_0268u32 as i32);
/// `STATUS_NOT_SUPPORTED`；IKsControl 桩函数与历史 C++ pin 一样返回它。
pub const STATUS_NOT_SUPPORTED: HRESULT = HRESULT(0xC000_00BBu32 as i32);

pub const fn failed(hr: HRESULT) -> bool {
    hr.0 < 0
}

pub const fn succeeded(hr: HRESULT) -> bool {
    hr.0 >= 0
}

// ---------- GUID ----------

pub const CLSID_VCAM: GUID = GUID::from_u128(0xB84C7E21_4A5F_4E2B_9C11_5A3D7F01229A);
pub const IID_IUNKNOWN: GUID = GUID::from_u128(0x0000_0000_0000_0000_C000_0000_0000_0046);
pub const IID_IPERSIST: GUID = GUID::from_u128(0x0000_010C_0000_0000_C000_0000_0000_0046);
pub const IID_ISpecifyPropertyPages: GUID = GUID::from_u128(0xB196B28B_BAB4_101A_B69C_00AA00341D07);
pub const IID_IKsControl: GUID = GUID::from_u128(0x28F54685_06FD_11D2_B27A_00A0C9223196);
pub const IID_IKsPin: GUID = GUID::from_u128(0xB61178D1_A2D9_11CF_9E53_00AA00A216A1);
pub const AMPROPSETID_Pin: GUID = GUID::from_u128(0x9B00F101_1567_11D1_B3F1_00AA003761C5);
pub const PIN_CATEGORY_CAPTURE: GUID = GUID::from_u128(0xFB6C4281_0353_11D1_905F_0000C0CC16BA);
pub const KSPROPERTY_SUPPORT_GET: u32 = 1;
pub const AMPROPERTY_PIN_CATEGORY: u32 = 0;

// 标准 DirectShow IID 在 `com.rs` 中直接取自 windows crate
// （`<IPin as Interface>::IID`），避免手抄 GUID 出错。

// ---------- 原始媒体结构体 ----------

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeI {
    pub cx: i32,
    pub cy: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BitmapInfoHeader {
    pub biSize: u32,
    pub biWidth: i32,
    pub biHeight: i32,
    pub biPlanes: u16,
    pub biBitCount: u16,
    pub biCompression: u32,
    pub biSizeImage: u32,
    pub biXPelsPerMeter: i32,
    pub biYPelsPerMeter: i32,
    pub biClrUsed: u32,
    pub biClrImportant: u32,
}

impl Default for BitmapInfoHeader {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VideoInfoHeader {
    pub rcSource: RawRect,
    pub rcTarget: RawRect,
    pub dwBitRate: u32,
    pub dwBitErrorRate: u32,
    pub AvgTimePerFrame: i64,
    pub bmiHeader: BitmapInfoHeader,
}

impl Default for VideoInfoHeader {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

/// `AM_MEDIA_TYPE`，`pUnk` 为裸指针（windows crate 的结构体用
/// `ManuallyDrop<Option<IUnknown>>` 包裹，布局相同）。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MediaType {
    pub majortype: GUID,
    pub subtype: GUID,
    pub bFixedSizeSamples: i32,
    pub bTemporalCompression: i32,
    pub lSampleSize: u32,
    pub formattype: GUID,
    pub pUnk: *mut c_void,
    pub cbFormat: u32,
    pub pbFormat: *mut u8,
}

impl Default for MediaType {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AllocatorPropertiesRaw {
    pub cBuffers: i32,
    pub cbBuffer: i32,
    pub cbAlign: i32,
    pub cbPrefix: i32,
}

/// `VIDEO_STREAM_CONFIG_CAPS`，与 strmif.h 的声明逐字段一致。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VideoStreamConfigCaps {
    pub guid: GUID,
    pub VideoStandard: u32,
    pub InputSize: SizeI,
    pub MinCroppingSize: SizeI,
    pub MaxCroppingSize: SizeI,
    pub CropGranularityX: i32,
    pub CropGranularityY: i32,
    pub CropAlignX: i32,
    pub CropAlignY: i32,
    pub MinOutputSize: SizeI,
    pub MaxOutputSize: SizeI,
    pub OutputGranularityX: i32,
    pub OutputGranularityY: i32,
    pub StretchTapsX: i32,
    pub StretchTapsY: i32,
    pub ShrinkTapsX: i32,
    pub ShrinkTapsY: i32,
    pub MinFrameInterval: i64,
    pub MaxFrameInterval: i64,
    pub MinBitsPerSecond: i32,
    pub MaxBitsPerSecond: i32,
}

impl Default for VideoStreamConfigCaps {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

/// `FILTER_INFO`（先 `achName[128]`，再 `IFilterGraph*`）。
#[repr(C)]
pub struct FilterInfoRaw {
    pub achName: [u16; 128],
    pub pGraph: *mut c_void,
}

/// `PIN_INFO`（`IBaseFilter*`、方向、`achName[128]`）。
#[repr(C)]
pub struct PinInfoRaw {
    pub pFilter: *mut c_void,
    pub dir: i32,
    pub achName: [u16; 128],
}

/// `CAUUID`（ocidl.h）。
#[repr(C)]
pub struct CaUuidRaw {
    pub cElems: u32,
    pub pElems: *mut GUID,
}

pub const PINDIR_OUTPUT: i32 = 1;
pub const PINDIR_INPUT: i32 = 0;
pub const AM_FILTER_MISC_FLAGS_IS_SOURCE: u32 = 2;
pub const STATE_STOPPED: i32 = 0;
pub const STATE_PAUSED: i32 = 1;
pub const STATE_RUNNING: i32 = 2;

// 布局保证。历史 C++ 侧使用 #pragma pack(1)；这里每个字段都自然对齐，
// 因此 repr(C) 产生完全相同的字节布局。
const _: () = assert!(size_of::<RawRect>() == 16);
const _: () = assert!(size_of::<SizeI>() == 8);
const _: () = assert!(size_of::<BitmapInfoHeader>() == 40);
const _: () = assert!(size_of::<VideoInfoHeader>() == 88);
const _: () = assert!(size_of::<AllocatorPropertiesRaw>() == 16);
const _: () = assert!(size_of::<VideoStreamConfigCaps>() == 128);
const _: () = assert!(size_of::<CaUuidRaw>() == 16);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<MediaType>() == 88);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<FilterInfoRaw>() == 264);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<PinInfoRaw>() == 272);

// ---------- COM vtable 类型 ----------

#[repr(C)]
pub struct IUnknownVtbl {
    pub QueryInterface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
pub struct IBaseFilterVtbl {
    pub base__: IUnknownVtbl,
    pub GetClassID: unsafe extern "system" fn(*mut c_void, *mut GUID) -> HRESULT,
    pub Stop: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub Pause: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub Run: unsafe extern "system" fn(*mut c_void, i64) -> HRESULT,
    pub GetState: unsafe extern "system" fn(*mut c_void, u32, *mut i32) -> HRESULT,
    pub SetSyncSource: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    pub GetSyncSource: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub EnumPins: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub FindPin: unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> HRESULT,
    pub QueryFilterInfo: unsafe extern "system" fn(*mut c_void, *mut FilterInfoRaw) -> HRESULT,
    pub JoinFilterGraph: unsafe extern "system" fn(*mut c_void, *mut c_void, *const u16) -> HRESULT,
    pub QueryPinVersion: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
    pub QueryVendorInfo: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
}

#[repr(C)]
pub struct IAMFilterMiscFlagsVtbl {
    pub base__: IUnknownVtbl,
    pub GetMiscFlags: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
pub struct IPinVtbl {
    pub base__: IUnknownVtbl,
    pub Connect: unsafe extern "system" fn(*mut c_void, *mut c_void, *const MediaType) -> HRESULT,
    pub ReceiveConnection:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *const MediaType) -> HRESULT,
    pub Disconnect: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub ConnectedTo: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub ConnectionMediaType: unsafe extern "system" fn(*mut c_void, *mut MediaType) -> HRESULT,
    pub QueryPinInfo: unsafe extern "system" fn(*mut c_void, *mut PinInfoRaw) -> HRESULT,
    pub QueryDirection: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
    pub QueryId: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
    pub QueryAccept: unsafe extern "system" fn(*mut c_void, *const MediaType) -> HRESULT,
    pub EnumMediaTypes: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub QueryInternalConnections:
        unsafe extern "system" fn(*mut c_void, *mut *mut c_void, *mut u32) -> HRESULT,
    pub EndOfStream: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub BeginFlush: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub EndFlush: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub NewSegment: unsafe extern "system" fn(*mut c_void, i64, i64, f64) -> HRESULT,
}

#[repr(C)]
pub struct IAMStreamConfigVtbl {
    pub base__: IUnknownVtbl,
    pub SetFormat: unsafe extern "system" fn(*mut c_void, *const MediaType) -> HRESULT,
    pub GetFormat: unsafe extern "system" fn(*mut c_void, *mut *mut MediaType) -> HRESULT,
    pub GetNumberOfCapabilities:
        unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32) -> HRESULT,
    pub GetStreamCaps:
        unsafe extern "system" fn(*mut c_void, i32, *mut *mut MediaType, *mut u8) -> HRESULT,
}

#[repr(C)]
pub struct IKsPropertySetVtbl {
    pub base__: IUnknownVtbl,
    pub Set: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        u32,
        *const c_void,
        u32,
        *const c_void,
        u32,
    ) -> HRESULT,
    pub Get: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        u32,
        *const c_void,
        u32,
        *mut c_void,
        u32,
        *mut u32,
    ) -> HRESULT,
    pub QuerySupported:
        unsafe extern "system" fn(*mut c_void, *const GUID, u32, *mut u32) -> HRESULT,
}

#[repr(C)]
pub struct ISpecifyPropertyPagesVtbl {
    pub base__: IUnknownVtbl,
    pub GetPages: unsafe extern "system" fn(*mut c_void, *mut CaUuidRaw) -> HRESULT,
}

#[repr(C)]
pub struct IKsControlVtbl {
    pub base__: IUnknownVtbl,
    pub KsProperty: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *mut c_void,
        u32,
        *mut u32,
    ) -> HRESULT,
    pub KsMethod: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *mut c_void,
        u32,
        *mut u32,
    ) -> HRESULT,
    pub KsEvent: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *mut c_void,
        u32,
        *mut u32,
    ) -> HRESULT,
}

/// IKsPin vtable，按历史 C++ 滤镜声明（旧式 ksproxy 布局）。
#[repr(C)]
pub struct IKsPinVtbl {
    pub base__: IUnknownVtbl,
    pub KsQueryMediums: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub KsQueryInterfaces: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub KsCreateSinkPinHandle: unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT,
    pub KsGetCurrentCommunication:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> HRESULT,
    pub KsPropagateAcquire: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub KsDeliver: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> HRESULT,
    pub KsMediaSamplesCompleted: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    pub KsPeekAllocator: unsafe extern "system" fn(*mut c_void, u32) -> *mut c_void,
    pub KsReceiveAllocator: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    pub KsRenegotiate: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub KsPassEventToFilter: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    pub KsSetPinFramingCache:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void, u32) -> HRESULT,
}

#[repr(C)]
pub struct IEnumPinsVtbl {
    pub base__: IUnknownVtbl,
    pub Next: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void, *mut u32) -> HRESULT,
    pub Skip: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    pub Reset: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub Clone: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
pub struct IEnumMediaTypesVtbl {
    pub base__: IUnknownVtbl,
    pub Next: unsafe extern "system" fn(*mut c_void, u32, *mut *mut MediaType, *mut u32) -> HRESULT,
    pub Skip: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    pub Reset: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub Clone: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
pub struct IClassFactoryVtbl {
    pub base__: IUnknownVtbl,
    pub CreateInstance: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        *const GUID,
        *mut *mut c_void,
    ) -> HRESULT,
    pub LockServer: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
}

// ---------- 原始 COM 辅助函数 ----------

/// 通过外部接口指针调用 AddRef，不接管所有权。
///
/// # Safety
///
/// `ptr` 必须为空，或为有效 COM 接口指针：其 vtable 可读、`AddRef` 项可
/// 调用，且对象在调用期间保持存活。
pub unsafe fn com_addref(ptr: *mut c_void) -> u32 {
    if ptr.is_null() {
        return 0;
    }
    let vtbl = *(ptr as *mut *const IUnknownVtbl);
    ((*vtbl).AddRef)(ptr)
}

/// 通过外部接口指针调用 Release。
///
/// # Safety
///
/// `ptr` 必须为空，或为有效 COM 接口指针：其 vtable 可读、`Release` 项可
/// 调用。该调用可能销毁对象；除非调用方还持有另一个引用，否则此后不得再
/// 使用 `ptr`。
pub unsafe fn com_release(ptr: *mut c_void) -> u32 {
    if ptr.is_null() {
        return 0;
    }
    let vtbl = *(ptr as *mut *const IUnknownVtbl);
    ((*vtbl).Release)(ptr)
}

// ---------- 小工具：字符串 / 内存 ----------

/// 转换成带结尾 NUL 的 UTF-16，符合 Win32 *W API 的期望格式。
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(core::iter::once(0)).collect()
}

/// 把 `src`（必须 NUL 结尾，或长度不超过目标缓冲）拷贝进固定大小的 Win32
/// 名称缓冲，安全截断，并保证结尾 NUL。
pub fn copy_name(dst: &mut [u16], src: &[u16]) {
    let limit = dst.len().saturating_sub(1);
    let count = src.len().min(limit);
    dst[..count].copy_from_slice(&src[..count]);
    dst[count] = 0;
    for slot in dst.iter_mut().skip(count + 1) {
        *slot = 0;
    }
}

/// 比较 NUL 结尾的 UTF-16 字符串是否相等（`wcslen`+`wcscmp` 的反向组合）。
///
/// # Safety
///
/// `ptr` 必须为空，或指向一个在调用期间保持可读的、NUL 结尾的 UTF-16
/// 字符串。
pub unsafe fn wide_eq(ptr: *const u16, expected: &[u16]) -> bool {
    if ptr.is_null() {
        return false;
    }
    let mut i = 0usize;
    loop {
        let c = *ptr.add(i);
        let Some(&want) = expected.get(i) else {
            return false;
        };
        if c != want {
            return false;
        }
        if c == 0 {
            return true;
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_constants_match_text_forms() {
        assert_eq!(
            CLSID_VCAM,
            GUID::from_u128(0xB84C7E21_4A5F_4E2B_9C11_5A3D7F01229A)
        );
        // {B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}
        assert_eq!(CLSID_VCAM.data1, 0xB84C7E21);
        assert_eq!(CLSID_VCAM.data2, 0x4A5F);
        assert_eq!(CLSID_VCAM.data3, 0x4E2B);
        assert_eq!(
            CLSID_VCAM.data4,
            [0x9C, 0x11, 0x5A, 0x3D, 0x7F, 0x01, 0x22, 0x9A]
        );
    }

    #[test]
    fn hresult_values_match_sdk() {
        assert_eq!(S_OK.0, 0);
        assert_eq!(VFW_S_CANT_CUE.0, 0x00040268);
        assert_eq!(VFW_E_TYPE_NOT_ACCEPTED.0, 0x8004022A_u32 as i32);
        assert_eq!(VFW_E_NO_TRANSPORT.0, 0x80040266_u32 as i32);
        assert_eq!(VFW_E_SIZENOTSET.0, 0x80040212_u32 as i32);
        assert_eq!(SELFREG_E_CLASS.0, 0x80040201_u32 as i32);
        assert_eq!(E_PROP_ID_UNSUPPORTED.0, 0x80070490_u32 as i32);
        assert!(succeeded(VFW_S_CANT_CUE));
        assert!(failed(VFW_E_NOT_FOUND));
    }

    #[test]
    fn wide_helpers() {
        let w = wide("Capture");
        assert_eq!(
            w,
            vec![
                'C' as u16, 'a' as u16, 'p' as u16, 't' as u16, 'u' as u16, 'r' as u16, 'e' as u16,
                0
            ]
        );
        let mut dst = [0xFFFFu16; 8];
        copy_name(&mut dst, &w);
        assert_eq!(&dst[..8], &[67, 97, 112, 116, 117, 114, 101, 0]);
        let mut small = [0xFFFFu16; 4];
        copy_name(&mut small, &w);
        assert_eq!(small, [67, 97, 112, 0]);
        assert!(unsafe { wide_eq(w.as_ptr(), &wide("Capture")) });
        assert!(!unsafe { wide_eq(w.as_ptr(), &wide("CaptureX")) });
    }
}
