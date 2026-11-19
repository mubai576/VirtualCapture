//! DXGI Desktop Duplication（`sender/dda.h` 的 Rust 实现）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::gpu::GpuConverter;
use super::util::{create_device, create_staging_texture, with_mapped_texture};
use crate::SourceConfig;
use crate::transform::{Transform, TransformMode, bgra_to_nv12};

/// 等待新图像的总预算，保证 `capture` 的 false 分支不超过约 20 ms。
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(20);

pub(crate) struct DdaSource {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    staging_width: u32,
    staging_height: u32,
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
    transform_mode: TransformMode,
    gpu: Option<GpuConverter>,
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

        let gpu = if config.gpu_convert {
            Some(
                GpuConverter::new(&device.device, &device.context, width, height).context(
                    "gpu_convert requested but D3D11 GPU converter initialization failed",
                )?,
            )
        } else {
            None
        };

        Ok(Self {
            device: device.device,
            context: device.context,
            duplication,
            staging: None,
            staging_width: 0,
            staging_height: 0,
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

            if let Some(gpu) = &self.gpu {
                gpu.convert(&texture, &transform, destination)
                    .context("gpu_convert failed; not falling back to CPU")?;
            } else {
                self.convert_cpu(&texture, &transform, destination)?;
            }
            return Ok(true);
        }
    }

    fn convert_cpu(
        &mut self,
        texture: &ID3D11Texture2D,
        transform: &Transform,
        destination: &mut [u8],
    ) -> Result<()> {
        let copy = transform.copy_rect(self.transform_mode, self.source_width, self.source_height);
        if !copy.valid() {
            bail!("DDA copy rectangle is empty");
        }
        self.ensure_staging(copy.w, copy.h)?;
        let staging = self
            .staging
            .as_ref()
            .context("DDA staging texture missing")?;
        let source_box = D3D11_BOX {
            left: copy.x,
            top: copy.y,
            front: 0,
            right: copy.x + copy.w,
            bottom: copy.y + copy.h,
            back: 1,
        };
        unsafe {
            self.context
                .CopySubresourceRegion(staging, 0, 0, 0, 0, texture, 0, Some(&source_box));
        }
        let identity;
        let sampling = if self.transform_mode == TransformMode::Crop {
            identity = Transform::identity(self.output_width, self.output_height);
            &identity
        } else {
            transform
        };
        with_mapped_texture(&self.context, staging, copy.h, |data, pitch| {
            bgra_to_nv12(data, pitch, sampling, destination)
        })
    }

    fn ensure_staging(&mut self, width: u32, height: u32) -> Result<()> {
        if self.staging.is_some() && self.staging_width == width && self.staging_height == height {
            return Ok(());
        }
        self.staging = Some(create_staging_texture(
            &self.device,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
        )?);
        self.staging_width = width;
        self.staging_height = height;
        Ok(())
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
