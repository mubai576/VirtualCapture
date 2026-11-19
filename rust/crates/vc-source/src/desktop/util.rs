//! Windows 共享工具：D3D11 硬件设备与适配器名。
//!
//! 桌面采集只保留 GPU 路径，因此这里不再提供 CPU staging 纹理/映射辅助；
//! BGRA→NV12 的 CPU 版本仅作为 `transform` 中的测试参考实现。

use anyhow::{Context, Result};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
};
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
