//! DXGI 桌面复制与硬件 GPU 转换；旧 `sender/dda.h` 仅保留在 Git 历史。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::gpu::GpuConverter;
use super::util::create_device;
use crate::SourceConfig;
use crate::transform::{Transform, TransformMode};

/// 等待新图像的总预算，保证 `capture` 的 false 分支不超过约 20 ms。
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(20);

pub(crate) struct DdaSource {
    duplication: IDXGIOutputDuplication,
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
    transform_mode: TransformMode,
    /// 桌面采集固定走硬件 GPU 转换，没有 `Option` 或 CPU 兜底。
    gpu: GpuConverter,
    skipped: Arc<AtomicU64>,
    adapter_name: String,
    last_present: i64,
}

impl DdaSource {
    pub(crate) fn open(
        config: &SourceConfig,
        width: u32,
        height: u32,
        transform_mode: TransformMode,
        skipped: Arc<AtomicU64>,
    ) -> Result<Self> {
        let device = create_device()?;
        let adapter_name = device.adapter_name.clone();
        let output = unsafe { device.adapter.EnumOutputs(config.output) }
            .with_context(|| format!("DDA output {} is not available", config.output))?;
        let desc = unsafe { output.GetDesc() }.context("DDA IDXGIOutput::GetDesc failed")?;
        let source_width =
            i64::from(desc.DesktopCoordinates.right) - i64::from(desc.DesktopCoordinates.left);
        let source_height =
            i64::from(desc.DesktopCoordinates.bottom) - i64::from(desc.DesktopCoordinates.top);
        if source_width < 2 || source_height < 2 || source_width > 16384 || source_height > 16384 {
            bail!(
                "DDA output {} has unsupported size {source_width}x{source_height}",
                config.output
            );
        }
        let source_width = source_width as u32;
        let source_height = source_height as u32;
        Transform::new(source_width, source_height, width, height, transform_mode)?;

        let output1: IDXGIOutput1 = output
            .cast()
            .context("DDA output does not support IDXGIOutput1")?;
        let duplication = unsafe { output1.DuplicateOutput(&device.device) }.with_context(
            || {
                format!(
                    "IDXGIOutput1::DuplicateOutput failed for output {} (exclusive fullscreen or protected output?)",
                    config.output
                )
            },
        )?;
        let duplication_desc = unsafe { duplication.GetDesc() };
        if duplication_desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
            && duplication_desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
        {
            bail!(
                "DDA output {} is rotated; only unrotated outputs are supported",
                config.output
            );
        }

        // GPU 转换器在 duplication 建立后立即创建；失败即打开失败，不换 CPU/WARP。
        let gpu = GpuConverter::new(&device.device, &device.context, width, height).context(
            "DDA GPU 转换器初始化失败（桌面采集必须使用硬件 D3D11 GPU，不回退 CPU/WARP）",
        )?;

        Ok(Self {
            duplication,
            source_width,
            source_height,
            output_width: width,
            output_height: height,
            transform_mode,
            gpu,
            skipped,
            adapter_name,
            last_present: 0,
        })
    }

    pub(crate) fn source_dimensions(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    pub(crate) fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    pub(crate) fn capture(&mut self, destination: &mut [u8]) -> Result<bool> {
        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let timeout_ms = (deadline - now).as_millis().clamp(1, 20) as u32;
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match unsafe {
                self.duplication
                    .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
            } {
                Ok(()) => {}
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(error)
                    if error.code() == DXGI_ERROR_ACCESS_LOST
                        || error.code() == DXGI_ERROR_INVALID_CALL =>
                {
                    bail!("DDA output lost (mode change or access revoked): {error}")
                }
                Err(error) => bail!("DDA AcquireNextFrame failed: {error}"),
            }
            let frame = AcquiredFrame {
                duplication: self.duplication.clone(),
            };

            // 仅指针更新/重复 Present：没有新图像，继续在预算内等待下一帧。
            if info.LastPresentTime == 0 || info.LastPresentTime == self.last_present {
                self.skipped.fetch_add(1, Ordering::Relaxed);
                drop(frame);
                continue;
            }
            if info.AccumulatedFrames > 1 {
                self.skipped
                    .fetch_add(u64::from(info.AccumulatedFrames - 1), Ordering::Relaxed);
            }
            let resource = resource.context("DDA AcquireNextFrame returned no resource")?;
            let texture: ID3D11Texture2D = resource
                .cast()
                .context("DDA frame resource is not ID3D11Texture2D")?;
            let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut texture_desc) };
            if texture_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                bail!(
                    "DDA frame format {:?} is not B8G8R8A8_UNORM",
                    texture_desc.Format
                );
            }
            let transform = Transform::new(
                texture_desc.Width,
                texture_desc.Height,
                self.output_width,
                self.output_height,
                self.transform_mode,
            )
            .context("DDA source/output geometry is unsupported")?;
            self.source_width = texture_desc.Width;
            self.source_height = texture_desc.Height;
            self.last_present = info.LastPresentTime;

            self.gpu
                .convert(&texture, &transform, destination)
                .context("DDA GPU 转换失败；桌面采集不回退 CPU/WARP")?;
            return Ok(true);
        }
    }
}

/// 成功 AcquireNextFrame 后必须 ReleaseFrame；Drop 覆盖所有提前返回路径。
/// 持有 duplication 的克隆而不是借用，便于在转换期间继续可变借用采集器；
/// 克隆只增加 COM 引用计数，不改变同一个 duplication 的帧状态。
struct AcquiredFrame {
    duplication: IDXGIOutputDuplication,
}

impl Drop for AcquiredFrame {
    fn drop(&mut self) {
        unsafe {
            let _ = self.duplication.ReleaseFrame();
        }
    }
}
