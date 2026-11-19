//! D3D11 计算着色器 BGRA→NV12 转换；旧 `sender/dda_gpu.h` 仅保留在 Git 历史。
//!
//! 数学与 CPU 参考实现（`crate::transform::bgra_to_nv12`，仅测试/oracle）完全
//! 同构：Y/UV 系数、2×2 平均和右移全部一致。输入 `B8G8R8A8_UNORM` 通过
//! `Load` 精确取回 8 位整数（`value*255+0.5` 的 UNORM 反归一化对 8 位可逆），
//! 因此 GPU 结果应与 CPU 参考逐字节一致。桌面采集固定走本 GPU 路径。
//!
//! 任何初始化/编译/Map 失败都返回错误；调用方不得改走 CPU。

use std::ffi::c_void;

use anyhow::{Context, Result, bail};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_ENABLE_STRICTNESS, D3DCompile};
use windows::Win32::Graphics::Direct3D::{D3D_SRV_DIMENSION_TEXTURE2D, ID3DBlob, ID3DInclude};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_BUFFER_UAV,
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV, D3D11_UAV_DIMENSION_BUFFER,
    D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, ID3D11Buffer, ID3D11ClassLinkage, ID3D11ComputeShader, ID3D11Device,
    ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN};
use windows::core::PCSTR;

use crate::transform::Transform;

/// 与本模块整数参考公式对应的计算着色器。
const HLSL: &str = r#"
Texture2D<float4> g_src : register(t0);
RWStructuredBuffer<uint> g_y : register(u0);
RWStructuredBuffer<uint> g_uv : register(u1);
cbuffer CC : register(b0) {
  uint ox,oy,rw,rh; uint dx,dy,dw,dh; uint width,height,pad0,pad1;
};
int C8(int v){ return clamp(v,0,255); }
int3 RGB(uint x,uint y){
  if(x<dx || y<dy || x>=dx+dw || y>=dy+dh) return int3(0,0,0);
  float4 c=g_src.Load(int3(ox+(x-dx)*rw/dw,oy+(y-dy)*rh/dh,0));
  return int3(c.xyz*255.0f+0.5f);
}
[numthreads(64,1,1)]
void CSY(uint3 tid : SV_DispatchThreadID){
  uint index=tid.x+tid.y*4194240u;
  uint off=index*4, n=width*height;
  if(off>=n) return;
  uint packed=0;
  for(uint k=0;k<4 && off+k<n;k++){
    uint pos=off+k;
    int3 c=RGB(pos%width,pos/width);
    packed |= uint((77*c.x+150*c.y+29*c.z)>>8)<<(k*8);
  }
  g_y[index]=packed;
}
[numthreads(64,1,1)]
void CSU(uint3 tid : SV_DispatchThreadID){
  uint index=tid.x+tid.y*4194240u;
  uint off=index*4, n=width*height/2;
  if(off>=n) return;
  uint packed=0;
  for(uint k=0;k<2 && off+k*2<n;k++){
    uint pos=off+k*2, x=pos%width, y=pos/width*2;
    int3 c=(RGB(x,y)+RGB(x+1,y)+RGB(x,y+1)+RGB(x+1,y+1))>>2;
    uint u=uint(C8(((-43*c.x-84*c.y+127*c.z)>>8)+128));
    uint v=uint(C8(((127*c.x-106*c.y-21*c.z)>>8)+128));
    packed |= u<<(k*16); packed |= v<<(k*16+8);
  }
  g_uv[index]=packed;
}
"#;

const THREADS_PER_GROUP: u64 = 64;
/// DXGI/D3D11 `Dispatch` 的 X 上限（线程组数）。HLSL 用
/// `tid.y * (65535*64)` 作为跨行偏移，与此对应。
const MAX_GROUPS_X: u64 = 65_535;

/// 可复用的 GPU 转换器。持有 shader、UAV 与 CPU 回读 staging buffer。
pub(crate) struct GpuConverter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    cs_y: ID3D11ComputeShader,
    cs_u: ID3D11ComputeShader,
    y_buf: ID3D11Buffer,
    uv_buf: ID3D11Buffer,
    y_staging: ID3D11Buffer,
    uv_staging: ID3D11Buffer,
    y_uav: ID3D11UnorderedAccessView,
    uv_uav: ID3D11UnorderedAccessView,
    cbuf: ID3D11Buffer,
    y_n: u32,
    uv_n: u32,
    y_bytes: usize,
    uv_bytes: usize,
    width: u32,
    height: u32,
}

impl GpuConverter {
    pub(crate) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        if !(2..=8192).contains(&width) || !width.is_multiple_of(2) {
            bail!("gpu-convert output width must be an even value in 2..=8192 (got {width})");
        }
        if !(2..=8192).contains(&height) || !height.is_multiple_of(2) {
            bail!("gpu-convert output height must be an even value in 2..=8192 (got {height})");
        }
        let y_bytes = width as usize * height as usize;
        let uv_bytes = y_bytes / 2;
        let y_n = y_bytes.div_ceil(4) as u32;
        let uv_n = uv_bytes.div_ceil(4) as u32;

        let cs_y = compile_shader(device, "CSY")?;
        let cs_u = compile_shader(device, "CSU")?;
        let y_buf = create_structured_buffer(
            device,
            y_n,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            0,
        )?;
        let uv_buf = create_structured_buffer(
            device,
            uv_n,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            0,
        )?;
        let y_staging = create_structured_buffer(
            device,
            y_n,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?;
        let uv_staging = create_structured_buffer(
            device,
            uv_n,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?;
        let y_uav = create_uav(device, &y_buf, y_n)?;
        let uv_uav = create_uav(device, &uv_buf, uv_n)?;
        let cbuf = create_constant_buffer(device)?;

        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            cs_y,
            cs_u,
            y_buf,
            uv_buf,
            y_staging,
            uv_staging,
            y_uav,
            uv_uav,
            cbuf,
            y_n,
            uv_n,
            y_bytes,
            uv_bytes,
            width,
            height,
        })
    }

    /// 对 `source`（整张 BGRA 纹理）执行与 CPU 相同的几何采样并回读 NV12。
    pub(crate) fn convert(
        &self,
        source: &ID3D11Texture2D,
        transform: &Transform,
        destination: &mut [u8],
    ) -> Result<()> {
        if destination.len() < self.y_bytes + self.uv_bytes {
            bail!(
                "gpu-convert destination too small: need {}, got {}",
                self.y_bytes + self.uv_bytes,
                destination.len()
            );
        }
        let constants = [
            transform.ox,
            transform.oy,
            transform.rw,
            transform.rh,
            transform.dx,
            transform.dy,
            transform.dw,
            transform.dh,
            self.width,
            self.height,
            0,
            0,
        ];
        unsafe {
            self.context.UpdateSubresource(
                &self.cbuf,
                0,
                None,
                constants.as_ptr() as *const c_void,
                0,
                0,
            );
        }

        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            self.device
                .CreateShaderResourceView(source, Some(&srv_desc), Some(&mut srv))
        }
        .context("gpu-convert CreateShaderResourceView failed")?;
        let srv = srv.context("gpu-convert CreateShaderResourceView returned null")?;

        let srvs = [Some(srv.clone())];
        let uavs = [Some(self.y_uav.clone()), Some(self.uv_uav.clone())];
        let cbufs = [Some(self.cbuf.clone())];
        unsafe {
            self.context.CSSetConstantBuffers(0, Some(&cbufs));
            self.context.CSSetShaderResources(0, Some(&srvs));
            self.context
                .CSSetUnorderedAccessViews(0, 2, Some(uavs.as_ptr()), None);

            self.context.CSSetShader(&self.cs_y, None);
            let (gx, gy) = dispatch_grid(self.y_n);
            self.context.Dispatch(gx, gy, 1);

            self.context.CSSetShader(&self.cs_u, None);
            let (gx, gy) = dispatch_grid(self.uv_n);
            self.context.Dispatch(gx, gy, 1);

            let no_uavs: [Option<ID3D11UnorderedAccessView>; 2] = [None, None];
            let no_srvs: [Option<ID3D11ShaderResourceView>; 1] = [None];
            let no_cbufs: [Option<ID3D11Buffer>; 1] = [None];
            self.context
                .CSSetUnorderedAccessViews(0, 2, Some(no_uavs.as_ptr()), None);
            self.context.CSSetShaderResources(0, Some(&no_srvs));
            self.context.CSSetConstantBuffers(0, Some(&no_cbufs));
            self.context.CSSetShader(None::<&ID3D11ComputeShader>, None);

            self.context.CopyResource(&self.y_staging, &self.y_buf);
            self.context.CopyResource(&self.uv_staging, &self.uv_buf);
        }

        let (y_dst, uv_dst) =
            destination[..self.y_bytes + self.uv_bytes].split_at_mut(self.y_bytes);
        map_copy(&self.context, &self.y_staging, y_dst)?;
        map_copy(&self.context, &self.uv_staging, uv_dst)?;
        let _ = srv;
        Ok(())
    }
}

fn create_structured_buffer(
    device: &ID3D11Device,
    elements: u32,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    bind_flags: u32,
    cpu_access: u32,
) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: elements
            .checked_mul(4)
            .context("structured buffer size overflow")?,
        Usage: usage,
        BindFlags: bind_flags,
        CPUAccessFlags: cpu_access,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: 4,
    };
    let mut buffer: Option<ID3D11Buffer> = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }
        .context("CreateBuffer(structured) failed")?;
    buffer.context("CreateBuffer(structured) returned null")
}

fn create_uav(
    device: &ID3D11Device,
    buffer: &ID3D11Buffer,
    elements: u32,
) -> Result<ID3D11UnorderedAccessView> {
    let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_UNKNOWN,
        ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D11_BUFFER_UAV {
                FirstElement: 0,
                NumElements: elements,
                Flags: 0,
            },
        },
    };
    let mut view: Option<ID3D11UnorderedAccessView> = None;
    unsafe { device.CreateUnorderedAccessView(buffer, Some(&desc), Some(&mut view)) }
        .context("CreateUnorderedAccessView failed")?;
    view.context("CreateUnorderedAccessView returned null")
}

fn create_constant_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 48,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buffer: Option<ID3D11Buffer> = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }
        .context("CreateBuffer(constants) failed")?;
    buffer.context("CreateBuffer(constants) returned null")
}

fn compile_shader(device: &ID3D11Device, entry: &str) -> Result<ID3D11ComputeShader> {
    let entry_ptr = match entry {
        "CSY" => PCSTR::from_raw(c"CSY".as_ptr().cast()),
        "CSU" => PCSTR::from_raw(c"CSU".as_ptr().cast()),
        other => bail!("unknown compute shader entry {other}"),
    };
    let source_name = PCSTR::from_raw(c"vc-source".as_ptr().cast());
    let target = PCSTR::from_raw(c"cs_5_0".as_ptr().cast());
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = unsafe {
        D3DCompile(
            HLSL.as_ptr() as *const c_void,
            HLSL.len(),
            source_name,
            None,
            None::<&ID3DInclude>,
            entry_ptr,
            target,
            D3DCOMPILE_ENABLE_STRICTNESS,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(error) = result {
        bail!(
            "compute shader {entry} compile failed: {error}{}",
            blob_suffix(&errors)
        );
    }
    let code = code.context("D3DCompile returned no bytecode")?;
    let bytecode = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize())
    };
    let mut shader: Option<ID3D11ComputeShader> = None;
    unsafe { device.CreateComputeShader(bytecode, None::<&ID3D11ClassLinkage>, Some(&mut shader)) }
        .with_context(|| format!("CreateComputeShader({entry}) failed"))?;
    shader.with_context(|| format!("CreateComputeShader({entry}) returned null"))
}

fn blob_suffix(blob: &Option<ID3DBlob>) -> String {
    let Some(blob) = blob else {
        return String::new();
    };
    let bytes = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    };
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_matches('\0').trim();
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

fn dispatch_grid(elements: u32) -> (u32, u32) {
    let groups = u64::from(elements).div_ceil(THREADS_PER_GROUP);
    let x = groups.clamp(1, MAX_GROUPS_X) as u32;
    let y = groups.div_ceil(MAX_GROUPS_X).max(1) as u32;
    (x, y)
}

fn map_copy(
    context: &ID3D11DeviceContext,
    resource: &ID3D11Buffer,
    destination: &mut [u8],
) -> Result<()> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .context("gpu-convert Map(readback) failed")?;
    if mapped.pData.is_null() {
        unsafe { context.Unmap(resource, 0) };
        bail!("gpu-convert Map(readback) returned null");
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            mapped.pData as *const u8,
            destination.as_mut_ptr(),
            destination.len(),
        );
        context.Unmap(resource, 0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::{TransformMode, bgra_to_nv12};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

    #[test]
    fn dispatch_grid_matches_native_limits() {
        assert_eq!(dispatch_grid(102_400), (1_600, 1));
        assert_eq!(dispatch_grid(65_535 * 64), (65_535, 1));
        assert_eq!(dispatch_grid(65_535 * 64 + 4), (65_535, 2));
        // 8192×8192 的 Y 平面 uint 数：需要 262144 组，按 65535 上限分 5 行。
        let y_bytes = 8192u32 * 8192u32;
        assert_eq!(dispatch_grid(y_bytes.div_ceil(4)), (65_535, 5));
    }

    fn bgra_pattern(width: u32, height: u32) -> Vec<u8> {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height {
            for x in 0..width {
                let offset = (y as usize * width as usize + x as usize) * 4;
                data[offset] = ((x * 7 + y * 13) % 255) as u8;
                data[offset + 1] = ((x * 11 + y * 3) % 253) as u8;
                data[offset + 2] = ((x * 17 + y * 5) % 251) as u8;
                data[offset + 3] = 255;
            }
        }
        data
    }

    fn upload_texture(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr() as *const c_void,
            SysMemPitch: width * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture)) }
            .expect("CreateTexture2D(test) failed");
        texture.expect("CreateTexture2D(test) returned null")
    }

    /// GPU 路径必须与 CPU 路径逐字节一致；无硬件 D3D11 时跳过。
    #[test]
    fn gpu_conversion_matches_cpu() {
        let Ok(context) = super::super::util::create_device() else {
            eprintln!("跳过：没有可用的硬件 D3D11 设备");
            return;
        };
        let converter = GpuConverter::new(&context.device, &context.context, 64, 64)
            .expect("GPU converter init");
        let cases = [
            (96u32, 80u32, TransformMode::Crop),
            (320, 200, TransformMode::Fit),
            (96, 80, TransformMode::Fill),
            (100, 60, TransformMode::Stretch),
        ];
        for (width, height, mode) in cases {
            let source = bgra_pattern(width, height);
            let texture = upload_texture(&context.device, width, height, &source);
            let transform = Transform::new(width, height, 64, 64, mode).unwrap();
            let mut expected = vec![0u8; 64 * 64 * 3 / 2];
            bgra_to_nv12(&source, width as usize * 4, &transform, &mut expected).unwrap();
            let mut actual = vec![0u8; expected.len()];
            converter
                .convert(&texture, &transform, &mut actual)
                .expect("GPU conversion failed");
            if actual != expected {
                let offset = actual
                    .iter()
                    .zip(&expected)
                    .position(|(a, e)| a != e)
                    .unwrap_or(0);
                panic!(
                    "GPU/CPU mismatch for {width}x{height} {mode:?} at byte {offset}: gpu={} cpu={}",
                    actual[offset], expected[offset]
                );
            }
        }
    }
}
