//! Windows 共享工具：D3D11 设备、适配器名、staging 纹理与映射。

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{DXGI_ADAPTER_DESC, IDXGIAdapter, IDXGIDevice};
use windows::core::Interface;

/// 一个 D3D11 硬件设备及其 DXGI 适配器。
pub(crate) struct D3dContext {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: IDXGIAdapter,
    pub adapter_name: String,
}

/// 建立硬件 D3D11 设备。固定使用 `HARDWARE`，失败即致命错误（不静默换 WARP）。
pub(crate) fn create_device() -> Result<D3dContext> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None::<&[windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL]>,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice(HARDWARE) failed")?;
    let device = device.context("D3D11CreateDevice returned no device")?;
    let context = context.context("D3D11CreateDevice returned no immediate context")?;
    let dxgi: IDXGIDevice = device
        .cast()
        .context("D3D11 device is not an IDXGIDevice")?;
    let adapter = unsafe { dxgi.GetAdapter() }.context("IDXGIDevice::GetAdapter failed")?;
    let adapter_name = match unsafe { adapter.GetDesc() } {
        Ok(desc) => decode_adapter_name(&desc),
        Err(_) => "unknown".into(),
    };
    Ok(D3dContext {
        device,
        context,
        adapter,
        adapter_name,
    })
}

fn decode_adapter_name(desc: &DXGI_ADAPTER_DESC) -> String {
    let len = desc
        .Description
        .iter()
        .position(|&ch| ch == 0)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..len])
}

/// 建立 CPU 可读的 BGRA staging 纹理。
pub(crate) fn create_staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("CreateTexture2D(staging) failed")?;
    texture.context("CreateTexture2D(staging) returned no texture")
}

/// 作用域结束时 Unmap。覆盖早退、`?` 与 unwind，避免映射泄漏。
struct MappedTexture<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}

impl Drop for MappedTexture<'_> {
    fn drop(&mut self) {
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

/// Map 一张 staging 纹理，把连续 `rows` 行的只读视图交给 `f`，之后必定 Unmap。
///
/// 视图长度为 `RowPitch * rows`，因此调用方可以按行跨度直接转换。
pub(crate) fn with_mapped_texture<R>(
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    rows: u32,
    f: impl FnOnce(&[u8], usize) -> Result<R>,
) -> Result<R> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.Map(
            texture,
            0,
            windows::Win32::Graphics::Direct3D11::D3D11_MAP_READ,
            0,
            Some(&mut mapped),
        )
    }
    .context("Map(staging) failed")?;
    let _mapped = MappedTexture { context, texture };
    if mapped.pData.is_null() {
        bail!("Map(staging) returned a null pointer");
    }
    let pitch = mapped.RowPitch as usize;
    let Some(len) = pitch.checked_mul(rows as usize) else {
        bail!("staging texture row pitch overflow");
    };
    let data = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, len) };
    f(data, pitch)
}
