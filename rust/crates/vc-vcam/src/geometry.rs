//! Negotiated NV12 stream geometry.
//!
//! Mirrors `VCStream` in `common/stream.h` for the one field group the DShow
//! filter exposes: width, height and fps.  The geometry is immutable for a
//! filter instance; a receiver session that publishes a different size must be
//! picked up by reopening the downstream device (same rule as the C++ filter).

/// Default / v2-compatible geometry: 640x640 @ 240 fps, 614400 bytes per frame.
pub const DEFAULT_WIDTH: u32 = 640;
pub const DEFAULT_HEIGHT: u32 = 640;
pub const DEFAULT_FPS: u32 = 240;
pub const MAX_DIMENSION: u32 = 8192;
pub const MAX_FPS: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamGeometry {
    width: u32,
    height: u32,
    fps: u32,
}

impl Default for StreamGeometry {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl StreamGeometry {
    pub const DEFAULT: Self = Self {
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        fps: DEFAULT_FPS,
    };

    /// Validates the session constraints enforced by `vc_shm_valid`:
    /// even dimensions in 2..=8192 and fps in 1..=1000.
    pub const fn new(width: u32, height: u32, fps: u32) -> Option<Self> {
        if width < 2 || height < 2 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return None;
        }
        if width & 1 != 0 || height & 1 != 0 {
            return None;
        }
        if fps < 1 || fps > MAX_FPS {
            return None;
        }
        Some(Self { width, height, fps })
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }

    pub const fn fps(self) -> u32 {
        self.fps
    }

    /// NV12 packed frame size: width * height * 3 / 2.
    pub const fn frame_bytes(self) -> u32 {
        self.width * self.height * 3 / 2
    }

    pub const fn y_bytes(self) -> u32 {
        self.width * self.height
    }

    pub const fn uv_bytes(self) -> u32 {
        self.width * self.height / 2
    }

    /// DShow `AvgTimePerFrame` in 100 ns units (integer division like the C++ filter).
    pub const fn frame_100ns(self) -> i64 {
        10_000_000 / self.fps as i64
    }

    /// Pacing period in ns, rounded up to a whole nanosecond (C++ `FRAME_NS`).
    pub const fn frame_ns(self) -> u64 {
        (1_000_000_000u64).div_ceil(self.fps as u64)
    }

    /// Bytes per second at the negotiated geometry, saturated like the C++ `dwBitRate`.
    pub const fn bits_per_second(self) -> u64 {
        self.frame_bytes() as u64 * 8 * self.fps as u64
    }

    /// `VIDEOINFOHEADER::dwBitRate` value (DWORD saturated).
    pub const fn dw_bit_rate(self) -> u32 {
        let bps = self.bits_per_second();
        if bps > u32::MAX as u64 {
            u32::MAX
        } else {
            bps as u32
        }
    }

    /// `VIDEO_STREAM_CONFIG_CAPS::Min/MaxBitsPerSecond` value (LONG saturated).
    pub const fn caps_bits_per_second(self) -> i32 {
        let bps = self.bits_per_second();
        if bps > i32::MAX as u64 {
            i32::MAX
        } else {
            bps as i32
        }
    }

    /// True when a media type / SHM header geometry pair must be treated as a
    /// session change: the downstream device has to be reopened.
    pub const fn differs(self, other: Self) -> bool {
        self.width != other.width || self.height != other.height || self.fps != other.fps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_v2_geometry() {
        let g = StreamGeometry::DEFAULT;
        assert_eq!((g.width(), g.height(), g.fps()), (640, 640, 240));
        assert_eq!(g.frame_bytes(), 614_400);
        assert_eq!(g.y_bytes(), 409_600);
        assert_eq!(g.uv_bytes(), 204_800);
        assert_eq!(g.frame_100ns(), 41_666);
        assert_eq!(g.frame_ns(), 4_166_667);
    }

    #[test]
    fn geometry_validation_matches_shm_rules() {
        assert!(StreamGeometry::new(2, 2, 1).is_some());
        assert!(StreamGeometry::new(416, 416, 60).is_some());
        assert!(StreamGeometry::new(8192, 8192, 1000).is_some());
        assert!(StreamGeometry::new(0, 640, 240).is_none());
        assert!(StreamGeometry::new(640, 0, 240).is_none());
        assert!(StreamGeometry::new(641, 640, 240).is_none());
        assert!(StreamGeometry::new(640, 639, 240).is_none());
        assert!(StreamGeometry::new(8194, 640, 240).is_none());
        assert!(StreamGeometry::new(640, 640, 0).is_none());
        assert!(StreamGeometry::new(640, 640, 1001).is_none());
    }

    #[test]
    fn frame_layout_and_rates() {
        let g = StreamGeometry::new(416, 416, 60).unwrap();
        assert_eq!(g.frame_bytes(), 416 * 416 * 3 / 2);
        assert_eq!(g.y_bytes() + g.uv_bytes(), g.frame_bytes());
        assert_eq!(g.frame_100ns(), 166_666);
        assert_eq!(g.frame_ns(), 16_666_667);
        assert_eq!(g.bits_per_second(), 416u64 * 416 * 3 / 2 * 8 * 60);
    }

    #[test]
    fn dw_bit_rate_saturates_at_dword() {
        let huge = StreamGeometry::new(8192, 8192, 1000).unwrap();
        assert_eq!(huge.dw_bit_rate(), u32::MAX);
        assert_eq!(huge.caps_bits_per_second(), i32::MAX);
        assert_eq!(StreamGeometry::DEFAULT.dw_bit_rate(), 1_179_648_000);
    }

    #[test]
    fn differs_compares_all_three_fields() {
        let a = StreamGeometry::DEFAULT;
        let b = StreamGeometry::new(640, 640, 239).unwrap();
        let c = StreamGeometry::new(642, 640, 240).unwrap();
        assert!(!a.differs(a));
        assert!(a.differs(b));
        assert!(a.differs(c));
    }
}
