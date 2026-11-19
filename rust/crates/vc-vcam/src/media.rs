//! 媒体类型辅助函数，对应历史 C++ 实现 `vcam/vcam.cpp` 中的
//! `VCamCreateType`、`VCamCopyType`、`VCamTypeMatches` 与 `GetStreamCaps`。
//!
//! 只公布协商会话几何下的 `FORMAT_VideoInfo` + NV12。滤镜实例绝不静默
//! 改变尺寸或 fps。

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;

use windows::Win32::Media::MediaFoundation::{
    FORMAT_VideoInfo, MEDIASUBTYPE_NV12, MEDIATYPE_Video,
};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};
use windows::core::GUID;

use crate::abi::{
    BitmapInfoHeader, E_OUTOFMEMORY, MediaType, RawRect, S_OK, VideoInfoHeader,
    VideoStreamConfigCaps, com_addref, com_release,
};
use crate::geometry::StreamGeometry;

pub const GUID_NULL: GUID = GUID::from_u128(0);
/// NV12 的 FourCC 值，即 `MAKEFOURCC('N','V','1','2')`。
pub const FOURCC_NV12: u32 = 0x3231_564E;

pub const VIDEOINFOHEADER_SIZE: u32 = 88;

/// 用 CoTaskMemAlloc 分配一对 `AM_MEDIA_TYPE` + `VIDEOINFOHEADER`，
/// 与 `VCamCreateType` 完全一致。调用方取得所有权，并用
/// [`free_media_type`] 释放。
///
/// # Safety
///
/// 返回的分配必须用 [`free_media_type`] 恰好释放一次；调用方不得通过
/// 其他方式释放。`geometry` 必须是有效的会话几何（凡经 [`StreamGeometry`]
/// 构造的值都满足）。
pub unsafe fn alloc_media_type(geometry: StreamGeometry) -> *mut MediaType {
    let mt = CoTaskMemAlloc(size_of::<MediaType>()) as *mut MediaType;
    if mt.is_null() {
        return ptr::null_mut();
    }
    ptr::write_bytes(mt as *mut u8, 0, size_of::<MediaType>());

    let vih = CoTaskMemAlloc(size_of::<VideoInfoHeader>()) as *mut VideoInfoHeader;
    if vih.is_null() {
        CoTaskMemFree(Some(mt as *const c_void));
        return ptr::null_mut();
    }
    ptr::write_bytes(vih as *mut u8, 0, size_of::<VideoInfoHeader>());

    let width = geometry.width() as i32;
    let height = geometry.height() as i32;
    (*vih).rcSource = RawRect {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    (*vih).rcTarget = RawRect {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    (*vih).dwBitRate = geometry.dw_bit_rate();
    (*vih).AvgTimePerFrame = geometry.frame_100ns();
    (*vih).bmiHeader = BitmapInfoHeader {
        biSize: size_of::<BitmapInfoHeader>() as u32,
        biWidth: width,
        biHeight: height,
        biPlanes: 1,
        biBitCount: 12,
        biCompression: FOURCC_NV12,
        biSizeImage: geometry.frame_bytes(),
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };

    (*mt).majortype = MEDIATYPE_Video;
    (*mt).subtype = MEDIASUBTYPE_NV12;
    (*mt).bFixedSizeSamples = 1; // TRUE（定长样本）
    (*mt).bTemporalCompression = 0; // FALSE（无时间压缩）
    (*mt).lSampleSize = geometry.frame_bytes();
    (*mt).formattype = FORMAT_VideoInfo;
    (*mt).pUnk = ptr::null_mut();
    (*mt).cbFormat = VIDEOINFOHEADER_SIZE;
    (*mt).pbFormat = vih as *mut u8;
    mt
}

/// 释放内部缓冲以及 `AM_MEDIA_TYPE` 本身（对应 C++ `VCamFreeType`）。
///
/// # Safety
///
/// `mt` 必须为空，或为 [`alloc_media_type`] 返回且尚未释放的分配。调用后
/// 指针即悬垂，不得再次使用。
pub unsafe fn free_media_type(mt: *mut MediaType) {
    if mt.is_null() {
        return;
    }
    clear_media_type(mt);
    CoTaskMemFree(Some(mt as *const c_void));
}

/// 只释放媒体类型的内部成员，结构体本身可内嵌（对应 C++ `VCamClearType`）。
///
/// # Safety
///
/// `mt` 必须为空，或指向可写、已初始化的 `MediaType`：其非空 `pbFormat`
/// 来自 CoTaskMemAlloc，非空 `pUnk` 持有由该媒体类型拥有的 COM 引用。
/// 本调用会把结构体清零，因此同一内部成员不得重复清理。
pub unsafe fn clear_media_type(mt: *mut MediaType) {
    if mt.is_null() {
        return;
    }
    if (*mt).cbFormat != 0 && !(*mt).pbFormat.is_null() {
        CoTaskMemFree(Some((*mt).pbFormat as *const c_void));
    }
    if !(*mt).pUnk.is_null() {
        com_release((*mt).pUnk);
    }
    ptr::write_bytes(mt as *mut u8, 0, size_of::<MediaType>());
}

/// 深拷贝到调用方提供的 `AM_MEDIA_TYPE`（对应 C++ `VCamCopyType`）。
///
/// # Safety
///
/// `dst` 必须为空，或指向可写、且不与 `src` 别名、也不持有存活内部成员的
/// `MediaType`（本调用会将其清零）。`src` 必须为空，或指向有效的已初始化
/// `MediaType`，其 `pbFormat`/`pUnk` 在调用期间保持有效。成功后新内部成员
/// 归调用方所有，须用 [`clear_media_type`] 释放。
pub unsafe fn copy_media_type(
    dst: *mut MediaType,
    src: *const MediaType,
) -> windows::core::HRESULT {
    if dst.is_null() || src.is_null() {
        return crate::abi::E_POINTER;
    }
    ptr::write_bytes(dst as *mut u8, 0, size_of::<MediaType>());
    (*dst).majortype = (*src).majortype;
    (*dst).subtype = (*src).subtype;
    (*dst).bFixedSizeSamples = (*src).bFixedSizeSamples;
    (*dst).bTemporalCompression = (*src).bTemporalCompression;
    (*dst).lSampleSize = (*src).lSampleSize;
    (*dst).formattype = (*src).formattype;
    if !(*src).pUnk.is_null() {
        com_addref((*src).pUnk);
        (*dst).pUnk = (*src).pUnk;
    }
    if (*src).cbFormat != 0 && !(*src).pbFormat.is_null() {
        let copy = CoTaskMemAlloc((*src).cbFormat as usize) as *mut u8;
        if copy.is_null() {
            if !(*dst).pUnk.is_null() {
                com_release((*dst).pUnk);
                (*dst).pUnk = ptr::null_mut();
            }
            return E_OUTOFMEMORY;
        }
        ptr::copy_nonoverlapping((*src).pbFormat, copy, (*src).cbFormat as usize);
        (*dst).pbFormat = copy;
        (*dst).cbFormat = (*src).cbFormat;
    }
    S_OK
}

/// 对应 C++ `VCamTypeMatches`：GUID_NULL 视为“不关心”；当带有
/// `FORMAT_VideoInfo` 载荷时，它必须匹配会话几何、帧时长、NV12 fourcc
/// 与精确帧大小。
///
/// # Safety
///
/// `mt` 必须为空，或指向可读的已初始化 `MediaType`；`pbFormat` 非空时，
/// 它必须在调用期间可读至少 `cbFormat` 字节。
pub unsafe fn matches_type(mt: *const MediaType, geometry: StreamGeometry) -> bool {
    if mt.is_null() {
        return false;
    }
    if (*mt).majortype != GUID_NULL && (*mt).majortype != MEDIATYPE_Video {
        return false;
    }
    if (*mt).subtype != GUID_NULL && (*mt).subtype != MEDIASUBTYPE_NV12 {
        return false;
    }
    if (*mt).formattype != GUID_NULL && (*mt).formattype != FORMAT_VideoInfo {
        return false;
    }
    if (*mt).formattype == FORMAT_VideoInfo
        && (*mt).cbFormat >= VIDEOINFOHEADER_SIZE
        && !(*mt).pbFormat.is_null()
    {
        let vih = (*mt).pbFormat as *const VideoInfoHeader;
        if (*vih).bmiHeader.biWidth as i64 != geometry.width() as i64
            || (*vih).bmiHeader.biHeight as i64 != geometry.height() as i64
        {
            return false;
        }
        if (*vih).AvgTimePerFrame != geometry.frame_100ns()
            || (*vih).bmiHeader.biSizeImage != geometry.frame_bytes()
        {
            return false;
        }
        if (*vih).bmiHeader.biCompression != FOURCC_NV12 {
            return false;
        }
    }
    true
}

/// 按 C++ pin 的 `GetStreamCaps` 填充调用方的 `VIDEO_STREAM_CONFIG_CAPS`
/// 缓冲，只广告一项不可变能力。
///
/// # Safety
///
/// `caps` 必须为空，或至少指向 `size_of::<VideoStreamConfigCaps>()` 个
/// 可写字节。
pub unsafe fn fill_stream_caps(geometry: StreamGeometry, caps: *mut u8) {
    if caps.is_null() {
        return;
    }
    ptr::write_bytes(caps, 0, size_of::<VideoStreamConfigCaps>());
    let c = caps as *mut VideoStreamConfigCaps;
    let width = geometry.width() as i32;
    let height = geometry.height() as i32;
    (*c).guid = FORMAT_VideoInfo;
    (*c).VideoStandard = 0; // AnalogVideo_None
    (*c).InputSize = crate::abi::SizeI {
        cx: width,
        cy: height,
    };
    (*c).MinCroppingSize = crate::abi::SizeI {
        cx: width,
        cy: height,
    };
    (*c).MaxCroppingSize = crate::abi::SizeI {
        cx: width,
        cy: height,
    };
    (*c).CropGranularityX = 1;
    (*c).CropGranularityY = 1;
    (*c).CropAlignX = 1;
    (*c).CropAlignY = 1;
    (*c).MinOutputSize = crate::abi::SizeI {
        cx: width,
        cy: height,
    };
    (*c).MaxOutputSize = crate::abi::SizeI {
        cx: width,
        cy: height,
    };
    (*c).OutputGranularityX = 1;
    (*c).OutputGranularityY = 1;
    (*c).MinFrameInterval = geometry.frame_100ns();
    (*c).MaxFrameInterval = geometry.frame_100ns();
    (*c).MinBitsPerSecond = geometry.caps_bits_per_second();
    (*c).MaxBitsPerSecond = geometry.caps_bits_per_second();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{PINDIR_OUTPUT, VideoStreamConfigCaps};

    fn stack_type(geometry: StreamGeometry) -> MediaType {
        let mut vih = VideoInfoHeader::default();
        vih.rcSource.right = geometry.width() as i32;
        vih.rcSource.bottom = geometry.height() as i32;
        vih.rcTarget = vih.rcSource;
        vih.dwBitRate = geometry.dw_bit_rate();
        vih.AvgTimePerFrame = geometry.frame_100ns();
        vih.bmiHeader.biSize = 40;
        vih.bmiHeader.biWidth = geometry.width() as i32;
        vih.bmiHeader.biHeight = geometry.height() as i32;
        vih.bmiHeader.biPlanes = 1;
        vih.bmiHeader.biBitCount = 12;
        vih.bmiHeader.biCompression = FOURCC_NV12;
        vih.bmiHeader.biSizeImage = geometry.frame_bytes();
        MediaType {
            majortype: MEDIATYPE_Video,
            subtype: MEDIASUBTYPE_NV12,
            bFixedSizeSamples: 1,
            lSampleSize: geometry.frame_bytes(),
            formattype: FORMAT_VideoInfo,
            cbFormat: VIDEOINFOHEADER_SIZE,
            pbFormat: Box::into_raw(Box::new(vih)) as *mut u8,
            ..Default::default()
        }
    }

    #[test]
    fn default_type_matches_own_geometry() {
        let g = StreamGeometry::DEFAULT;
        let mt = stack_type(g);
        assert!(unsafe { matches_type(&mt, g) });
        assert!(!unsafe { matches_type(&mt, StreamGeometry::new(416, 416, 60).unwrap()) });
        unsafe { drop(Box::from_raw(mt.pbFormat as *mut VideoInfoHeader)) };
    }

    #[test]
    fn type_matching_rejects_mutations() {
        let g = StreamGeometry::DEFAULT;
        let base = stack_type(g);
        unsafe {
            let mut mt = base;

            mt.bmiHeader_mut().bmiHeader.biWidth = 642;
            assert!(!matches_type(&mt, g));
            mt = base;
            mt.bmiHeader_mut().bmiHeader.biHeight = 638;
            assert!(!matches_type(&mt, g));
            mt = base;
            mt.bmiHeader_mut().AvgTimePerFrame = 41_667;
            assert!(!matches_type(&mt, g));
            mt = base;
            mt.bmiHeader_mut().bmiHeader.biSizeImage = 614_399;
            assert!(!matches_type(&mt, g));
            mt = base;
            mt.bmiHeader_mut().bmiHeader.biCompression = 0;
            assert!(!matches_type(&mt, g));
            mt = base;
            mt.subtype = GUID::from_u128(0x0000_0001_0000_0010_8000_00AA_0038_9B71);
            assert!(!matches_type(&mt, g));
            mt = base;
            mt.majortype = MEDIATYPE_Video;
            mt.formattype = GUID_NULL;
            assert!(matches_type(&mt, g));
            mt = base;
            mt.majortype = GUID_NULL;
            mt.subtype = GUID_NULL;
            mt.formattype = GUID_NULL;
            assert!(matches_type(&mt, g));
            mt = base;
            mt.majortype = GUID::from_u128(1);
            assert!(!matches_type(&mt, g));
        }
        unsafe { drop(Box::from_raw(base.pbFormat as *mut VideoInfoHeader)) };
    }

    #[test]
    fn allocated_type_round_trips_and_copies() {
        let g = StreamGeometry::new(416, 416, 60).unwrap();
        unsafe {
            let mt = alloc_media_type(g);
            assert!(!mt.is_null());
            assert!(matches_type(mt, g));
            let vih = (*mt).pbFormat as *const VideoInfoHeader;
            assert_eq!((*vih).bmiHeader.biWidth, 416);
            assert_eq!((*vih).bmiHeader.biHeight, 416);
            assert_eq!((*vih).bmiHeader.biCompression, FOURCC_NV12);
            assert_eq!((*vih).bmiHeader.biSizeImage, g.frame_bytes());
            assert_eq!((*vih).AvgTimePerFrame, g.frame_100ns());

            let mut copy = MediaType::default();
            assert_eq!(copy_media_type(&mut copy, mt), S_OK);
            assert!(matches_type(&copy, g));
            assert!(!copy.pbFormat.is_null());
            assert_ne!(copy.pbFormat, (*mt).pbFormat);
            clear_media_type(&mut copy);
            assert!(copy.pbFormat.is_null());
            assert_eq!(copy.cbFormat, 0);
            free_media_type(mt);
        }
    }

    #[test]
    fn stream_caps_advertise_one_fixed_capability() {
        let g = StreamGeometry::DEFAULT;
        let mut buf = [0u8; 128];
        unsafe {
            fill_stream_caps(g, buf.as_mut_ptr());
            let c = &*(buf.as_ptr() as *const VideoStreamConfigCaps);
            assert_eq!(c.guid, FORMAT_VideoInfo);
            assert_eq!(c.InputSize.cx, 640);
            assert_eq!(c.MinOutputSize.cy, 640);
            assert_eq!(c.MaxOutputSize.cx, 640);
            assert_eq!(c.CropGranularityX, 1);
            assert_eq!(c.OutputGranularityY, 1);
            assert_eq!(c.MinFrameInterval, 41_666);
            assert_eq!(c.MaxFrameInterval, 41_666);
            assert_eq!(c.MinBitsPerSecond, 1_179_648_000);
            assert_eq!(c.MaxBitsPerSecond, c.MinBitsPerSecond);
            assert_eq!(c.VideoStandard, 0);
        }
    }

    #[test]
    fn pin_direction_constant_matches_sdk() {
        assert_eq!(PINDIR_OUTPUT, 1);
    }

    // 小辅助函数：让变异测试保持易读。
    impl MediaType {
        unsafe fn bmiHeader_mut(&mut self) -> &mut VideoInfoHeader {
            &mut *(self.pbFormat as *mut VideoInfoHeader)
        }
    }
}
