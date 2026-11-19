//! Media-type helpers mirroring `VCamCreateType`, `VCamCopyType`,
//! `VCamTypeMatches` and `GetStreamCaps` from `vcam/vcam.cpp`.
//!
//! Only `FORMAT_VideoInfo` + NV12 at the negotiated session geometry is
//! advertised.  A filter instance never silently changes size or fps.

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
/// `MAKEFOURCC('N','V','1','2')`.
pub const FOURCC_NV12: u32 = 0x3231_564E;

pub const VIDEOINFOHEADER_SIZE: u32 = 88;

/// Allocates an `AM_MEDIA_TYPE` + `VIDEOINFOHEADER` pair with CoTaskMemAlloc,
/// exactly like `VCamCreateType`.  Caller owns the result and frees it with
/// [`free_media_type`].
///
/// # Safety
///
/// The returned allocation must be released exactly once with
/// [`free_media_type`]; the caller must not free it by any other means.
/// `geometry` must be a valid session geometry (all values built through
/// [`StreamGeometry`] satisfy this).
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
    (*mt).bFixedSizeSamples = 1; // TRUE
    (*mt).bTemporalCompression = 0; // FALSE
    (*mt).lSampleSize = geometry.frame_bytes();
    (*mt).formattype = FORMAT_VideoInfo;
    (*mt).pUnk = ptr::null_mut();
    (*mt).cbFormat = VIDEOINFOHEADER_SIZE;
    (*mt).pbFormat = vih as *mut u8;
    mt
}

/// Frees internals and the `AM_MEDIA_TYPE` itself (C++ `VCamFreeType`).
///
/// # Safety
///
/// `mt` must be null or an allocation returned by [`alloc_media_type`] that
/// has not been freed before.  After the call the pointer is dangling and
/// must not be used again.
pub unsafe fn free_media_type(mt: *mut MediaType) {
    if mt.is_null() {
        return;
    }
    clear_media_type(mt);
    CoTaskMemFree(Some(mt as *const c_void));
}

/// Frees only the internals of a possibly embedded media type
/// (C++ `VCamClearType`).
///
/// # Safety
///
/// `mt` must be null or point to a writable, initialized `MediaType` whose
/// non-null `pbFormat` comes from CoTaskMemAlloc and whose non-null `pUnk`
/// holds a COM reference owned by this media type.  The struct is zeroed, so
/// the same internals must not be cleared twice.
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

/// Deep copy into a caller-provided `AM_MEDIA_TYPE` (C++ `VCamCopyType`).
///
/// # Safety
///
/// `dst` must be null or point to a writable `MediaType` that does not alias
/// `src` and does not own live internals (they are zeroed by this call).
/// `src` must be null or point to a valid initialized `MediaType` whose
/// `pbFormat`/`pUnk` members stay valid for the call.  On success the caller
/// owns the new internals and must release them with [`clear_media_type`].
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

/// C++ `VCamTypeMatches`: GUID_NULL acts as "don't care"; when a
/// `FORMAT_VideoInfo` payload is present it must carry the session geometry,
/// frame duration, NV12 fourcc and exact frame size.
///
/// # Safety
///
/// `mt` must be null or point to a readable initialized `MediaType`; when
/// `pbFormat` is non-null it must address at least `cbFormat` readable bytes
/// for the duration of the call.
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

/// Fills the caller's `VIDEO_STREAM_CONFIG_CAPS` buffer exactly like
/// `CVMamPin::GetStreamCaps`, advertising one immutable capability.
///
/// # Safety
///
/// `caps` must be null or address at least
/// `size_of::<VideoStreamConfigCaps>()` writable bytes.
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

    // Small helper so the mutation test stays readable.
    impl MediaType {
        unsafe fn bmiHeader_mut(&mut self) -> &mut VideoInfoHeader {
            &mut *(self.pbFormat as *mut VideoInfoHeader)
        }
    }
}
