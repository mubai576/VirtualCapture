//! D3D11 计算着色器 BGRA→NV12 转换；旧 `sender/dda_gpu.h` 仅保留在 Git 历史。
//!
//! 数学与 CPU 参考实现（`crate::transform::bgra_to_nv12`，仅测试/oracle）完全
//! 同构：Y/UV 系数、2×2 平均和右移全部一致。输入 `B8G8R8A8_UNORM` 通过
//! `Load` 精确取回 8 位整数（`value*255+0.5` 的 UNORM 反归一化对 8 位可逆），
//! 因此 GPU 结果应与 CPU 参考逐字节一致。桌面采集固定走本 GPU 路径。
//!
//! 任何初始化/编译/Map 失败都返回错误；调用方不得改走 CPU。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::{D3D_SRV_DIMENSION_TEXTURE2D, ID3DBlob, ID3DInclude};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_BUFFER_UAV,
    D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RESOURCE_MISC_BUFFER_STRUCTURED, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV, D3D11_UAV_DIMENSION_BUFFER,
    D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, ID3D11Buffer, ID3D11ClassLinkage, ID3D11ComputeShader, ID3D11Device,
    ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::{PCSTR, PCWSTR};

use crate::transform::Transform;

/// 与本模块整数参考公式对应的计算着色器；Y/UV 合并写入同一 NV12 缓冲区。
const HLSL: &str = r#"
Texture2D<float4> g_src : register(t0);
RWStructuredBuffer<uint> g_nv12 : register(u0);
cbuffer CC : register(b0) {
  uint ox,oy,rw,rh; uint dx,dy,dw,dh; uint width,height,y_u32_offset,pad1;
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
  g_nv12[index]=packed;
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
  g_nv12[y_u32_offset+index]=packed;
}
[numthreads(8,8,1)]
void CSNV12(uint3 tid : SV_DispatchThreadID){
  uint width_u32=width>>2;
  uint half_h=height>>1;
  if(tid.x>=width_u32 || tid.y>=half_h) return;
  uint x0=tid.x<<2;
  uint y0=tid.y<<1;
  uint y1=y0+1;
  bool y0_in=(y0>=dy && y0<dy+dh);
  bool y1_in=(y1>=dy && y1<dy+dh);
  uint sy0=oy+((rh==dh)?(y0-dy):((y0-dy)*rh/dh));
  uint sy1=oy+((rh==dh)?(y1-dy):((y1-dy)*rh/dh));
  int3 r0[4];
  int3 r1[4];
  [unroll]
  for(uint k=0;k<4;k++){
    uint x=x0+k;
    bool x_in=(x>=dx && x<dx+dw);
    uint sx=ox+((rw==dw)?(x-dx):((x-dx)*rw/dw));
    if(x_in && y0_in){
      float4 c0=g_src.Load(int3(int(sx),int(sy0),0));
      r0[k]=int3(c0.xyz*255.0f+0.5f);
    }else{
      r0[k]=int3(0,0,0);
    }
    if(x_in && y1_in){
      float4 c1=g_src.Load(int3(int(sx),int(sy1),0));
      r1[k]=int3(c1.xyz*255.0f+0.5f);
    }else{
      r1[k]=int3(0,0,0);
    }
  }
  uint py0=0;
  uint py1=0;
  [unroll]
  for(uint k=0;k<4;k++){
    int3 a=r0[k];
    int3 b=r1[k];
    py0 |= uint((77*a.x+150*a.y+29*a.z)>>8)<<(k*8);
    py1 |= uint((77*b.x+150*b.y+29*b.z)>>8)<<(k*8);
  }
  g_nv12[y0*width_u32+tid.x]=py0;
  g_nv12[y1*width_u32+tid.x]=py1;
  uint puv=0;
  [unroll]
  for(uint k=0;k<2;k++){
    uint i=k*2;
    int3 c=(r0[i]+r0[i+1]+r1[i]+r1[i+1])>>2;
    uint u=uint(C8(((-43*c.x-84*c.y+127*c.z)>>8)+128));
    uint v=uint(C8(((127*c.x-106*c.y-21*c.z)>>8)+128));
    puv |= (u<<(k*16)) | (v<<(k*16+8));
  }
  g_nv12[y_u32_offset+tid.y*width_u32+tid.x]=puv;
}
"#;

const THREADS_PER_GROUP: u64 = 64;
/// DXGI/D3D11 `Dispatch` 的 X 上限（线程组数）。HLSL 用
/// `tid.y * (65535*64)` 作为跨行偏移，与此对应。
const MAX_GROUPS_X: u64 = 65_535;
/// 等待驱动完成已提交的回读；到期显式失败，不进入阻塞式 Map 或换用 CPU。
const READBACK_TIMEOUT: Duration = Duration::from_secs(1);
/// 先短时自旋（不触发 syscall），再让出当前核时间片覆盖 120–450 us 常见硬件回读，
/// 仅更长等待才进入私有高分辨率 waitable timer，避免过早落入 OS 定时器唤醒过冲。
const READBACK_FAST_POLL: Duration = Duration::from_micros(120);
const READBACK_YIELD_POLL: Duration = Duration::from_micros(450);
const READBACK_TIMER_WAIT_NS: u64 = 200_000;

enum ComputePipeline {
    Fused(ID3D11ComputeShader),
    Split {
        y: ID3D11ComputeShader,
        u: ID3D11ComputeShader,
    },
}

/// 可复用的 GPU 转换器。持有 shader、合并后的 NV12 UAV 与单份 CPU 回读 staging buffer。
pub(crate) struct GpuConverter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pipeline: ComputePipeline,
    nv12_buf: ID3D11Buffer,
    nv12_staging: ID3D11Buffer,
    nv12_uav: [Option<ID3D11UnorderedAccessView>; 1],
    cbuf: ID3D11Buffer,
    cbuf_bind: [Option<ID3D11Buffer>; 1],
    y_n: u32,
    uv_n: u32,
    y_bytes: usize,
    uv_bytes: usize,
    width: u32,
    height: u32,
    last_transform: Option<Transform>,
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
        // 宽高均为偶数，y_bytes 恒为 4 的倍数；合并写入同一 NV12 缓冲时 UV 起始 uint 精确对齐。
        let y_n = (y_bytes / 4) as u32;
        let uv_n = uv_bytes.div_ceil(4) as u32;
        let total_n = y_n
            .checked_add(uv_n)
            .context("gpu-convert buffer element count overflow")?;

        let pipeline = if width.is_multiple_of(4) {
            ComputePipeline::Fused(compile_shader(device, "CSNV12")?)
        } else {
            ComputePipeline::Split {
                y: compile_shader(device, "CSY")?,
                u: compile_shader(device, "CSU")?,
            }
        };
        let nv12_buf = create_structured_buffer(
            device,
            total_n,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            0,
        )?;
        let nv12_staging = create_structured_buffer(
            device,
            total_n,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?;
        let nv12_uav = create_uav(device, &nv12_buf, total_n)?;
        let cbuf = create_constant_buffer(device)?;
        let cbuf_bind = [Some(cbuf.clone())];

        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            pipeline,
            nv12_buf,
            nv12_staging,
            nv12_uav: [Some(nv12_uav)],
            cbuf,
            cbuf_bind,
            y_n,
            uv_n,
            y_bytes,
            uv_bytes,
            width,
            height,
            last_transform: None,
        })
    }

    /// 对 `source`（整张 BGRA 纹理）执行与 CPU 相同的几何采样并回读 NV12。
    pub(crate) fn convert(
        &mut self,
        source: &ID3D11Texture2D,
        transform: &Transform,
        destination: &mut [u8],
        stop: Option<&AtomicBool>,
    ) -> Result<bool> {
        if cancelled(stop) {
            return Ok(false);
        }
        let total_bytes = self.y_bytes + self.uv_bytes;
        if destination.len() < total_bytes {
            bail!(
                "gpu-convert destination too small: need {}, got {}",
                total_bytes,
                destination.len()
            );
        }
        if self.last_transform.as_ref() != Some(transform) {
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
                self.y_n,
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
            self.last_transform = Some(*transform);
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

        let srvs = [Some(srv)];
        unsafe {
            self.context.CSSetConstantBuffers(0, Some(&self.cbuf_bind));
            self.context.CSSetShaderResources(0, Some(&srvs));
            self.context
                .CSSetUnorderedAccessViews(0, 1, Some(self.nv12_uav.as_ptr()), None);

            match &self.pipeline {
                ComputePipeline::Fused(cs_nv12) => {
                    self.context.CSSetShader(cs_nv12, None);
                    let (gx, gy) = dispatch_grid_2d(self.width, self.height);
                    self.context.Dispatch(gx, gy, 1);
                }
                ComputePipeline::Split { y, u } => {
                    self.context.CSSetShader(y, None);
                    let (gx, gy) = dispatch_grid(self.y_n);
                    self.context.Dispatch(gx, gy, 1);

                    self.context.CSSetShader(u, None);
                    let (gx, gy) = dispatch_grid(self.uv_n);
                    self.context.Dispatch(gx, gy, 1);
                }
            }

            let no_uavs: [Option<ID3D11UnorderedAccessView>; 1] = [None];
            let no_srvs: [Option<ID3D11ShaderResourceView>; 1] = [None];
            let no_cbufs: [Option<ID3D11Buffer>; 1] = [None];
            self.context
                .CSSetUnorderedAccessViews(0, 1, Some(no_uavs.as_ptr()), None);
            self.context.CSSetShaderResources(0, Some(&no_srvs));
            self.context.CSSetConstantBuffers(0, Some(&no_cbufs));
            self.context.CSSetShader(None::<&ID3D11ComputeShader>, None);

            self.context
                .CopyResource(&self.nv12_staging, &self.nv12_buf);
            // 非阻塞 Map 不负责等待/提交驱动批次，先提交再按同一截止时间轮询。
            self.context.Flush();
        }

        let deadline = Instant::now() + READBACK_TIMEOUT;
        map_copy(
            &self.context,
            &self.nv12_staging,
            &mut destination[..total_bytes],
            stop,
            deadline,
        )
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
        "CSNV12" => PCSTR::from_raw(c"CSNV12".as_ptr().cast()),
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
            D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
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

fn dispatch_grid_2d(width: u32, height: u32) -> (u32, u32) {
    let width_u32 = width >> 2;
    let half_h = height >> 1;
    (width_u32.div_ceil(8).max(1), half_h.div_ceil(8).max(1))
}

fn cancelled(stop: Option<&AtomicBool>) -> bool {
    stop.is_some_and(|stop| stop.load(Ordering::Acquire))
}

struct ReadbackTimer(HANDLE);

impl ReadbackTimer {
    fn new() -> Self {
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .unwrap_or_default();
        Self(handle)
    }

    fn wait(&self, ns: u64) {
        let due = -(ns.div_ceil(100) as i64);
        if !self.0.is_invalid()
            && unsafe { SetWaitableTimer(self.0, &due, 0, None, None, false).is_ok() }
            && unsafe { WaitForSingleObject(self.0, 20) == WAIT_OBJECT_0 }
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

impl Drop for ReadbackTimer {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

fn wait_readback_step() {
    thread_local! {
        static TIMER: ReadbackTimer = ReadbackTimer::new();
    }
    TIMER.with(|timer| timer.wait(READBACK_TIMER_WAIT_NS));
}

/// 只轮询非阻塞操作；取消不展开任何在途原生调用，也不宣称已释放 GPU/DLL。
fn wait_readback(
    stop: Option<&AtomicBool>,
    deadline: Instant,
    mut try_map: impl FnMut() -> Result<bool>,
) -> Result<bool> {
    let started = Instant::now();
    loop {
        if cancelled(stop) {
            return Ok(false);
        }
        let now = Instant::now();
        if now >= deadline {
            bail!("gpu-convert readback timed out (DO_NOT_WAIT); no CPU/WARP fallback");
        }
        if try_map()? {
            return Ok(true);
        }
        let elapsed = now.duration_since(started);
        if elapsed < READBACK_FAST_POLL {
            for _ in 0..32 {
                std::hint::spin_loop();
            }
        } else if elapsed < READBACK_YIELD_POLL {
            std::thread::yield_now();
        } else {
            wait_readback_step();
        }
    }
}

fn map_copy(
    context: &ID3D11DeviceContext,
    resource: &ID3D11Buffer,
    destination: &mut [u8],
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<bool> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let ready = wait_readback(stop, deadline, || {
        match unsafe {
            context.Map(
                resource,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        } {
            Ok(()) => Ok(true),
            Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => Ok(false),
            Err(error) => Err(error).context("gpu-convert Map(readback, DO_NOT_WAIT) failed"),
        }
    })?;
    if !ready {
        return Ok(false);
    }
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
    Ok(true)
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
    fn readback_retries_only_until_ready() {
        let mut attempts = 0;
        assert!(
            wait_readback(None, Instant::now() + Duration::from_secs(1), || {
                attempts += 1;
                Ok(attempts == 3)
            })
            .unwrap()
        );
        assert_eq!(attempts, 3);
    }

    #[test]
    fn readback_cancellation_does_not_enter_or_repeat_map() {
        let stop = AtomicBool::new(true);
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(!wait_readback(Some(&stop), deadline, || panic!("cancelled Map")).unwrap());
        stop.store(false, Ordering::Release);
        let mut attempts = 0;
        assert!(
            !wait_readback(Some(&stop), deadline, || {
                attempts += 1;
                stop.store(true, Ordering::Release);
                Ok(false)
            })
            .unwrap()
        );
        assert_eq!(attempts, 1);
    }

    #[test]
    fn readback_fast_poll_retries_without_long_sleep() {
        let mut attempts = 0;
        let started = Instant::now();
        assert!(
            wait_readback(None, Instant::now() + Duration::from_secs(1), || {
                attempts += 1;
                Ok(attempts == 5)
            })
            .unwrap()
        );
        assert_eq!(attempts, 5);
        assert!(
            started.elapsed() < Duration::from_millis(15),
            "fast poll should not hit a 15.6 ms OS timer tick on brief retries"
        );
    }

    #[test]
    fn readback_timeout_and_native_error_are_fatal() {
        let mut attempts = 0;
        let error = wait_readback(None, Instant::now() + Duration::from_millis(10), || {
            attempts += 1;
            Ok(false)
        })
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(attempts > 0);
        let error = wait_readback(None, Instant::now() + Duration::from_secs(1), || {
            bail!("device removed")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "device removed");
    }

    #[test]
    fn dispatch_grid_matches_native_limits() {
        assert_eq!(dispatch_grid(102_400), (1_600, 1));
        assert_eq!(dispatch_grid(65_535 * 64), (65_535, 1));
        assert_eq!(dispatch_grid(65_535 * 64 + 4), (65_535, 2));
        // 8192×8192 的 Y 平面 uint 数：需要 262144 组，按 65535 上限分 5 行。
        let y_bytes = 8192u32 * 8192u32;
        assert_eq!(dispatch_grid(y_bytes.div_ceil(4)), (65_535, 5));

        assert_eq!(dispatch_grid_2d(64, 64), (2, 4));
        assert_eq!(dispatch_grid_2d(640, 640), (20, 40));
        assert_eq!(dispatch_grid_2d(8192, 8192), (256, 512));
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
        // 同时覆盖 width % 4 == 0 融合路径（64x64, 640x640）与 width % 4 == 2 回退路径（66x64, 2x2）。
        for (out_w, out_h, cases) in [
            (
                64u32,
                64u32,
                [
                    (96u32, 80u32, TransformMode::Crop),
                    (320, 200, TransformMode::Fit),
                    (96, 80, TransformMode::Fill),
                    (100, 60, TransformMode::Stretch),
                ],
            ),
            (
                640,
                640,
                [
                    (720u32, 680u32, TransformMode::Crop),
                    (320, 200, TransformMode::Fit),
                    (720, 680, TransformMode::Fill),
                    (100, 60, TransformMode::Stretch),
                ],
            ),
            (
                66,
                64,
                [
                    (96u32, 80u32, TransformMode::Crop),
                    (320, 200, TransformMode::Fit),
                    (96, 80, TransformMode::Fill),
                    (100, 60, TransformMode::Stretch),
                ],
            ),
            (
                2,
                2,
                [
                    (96u32, 80u32, TransformMode::Crop),
                    (320, 200, TransformMode::Fit),
                    (96, 80, TransformMode::Fill),
                    (100, 60, TransformMode::Stretch),
                ],
            ),
        ] {
            let mut converter = GpuConverter::new(&context.device, &context.context, out_w, out_h)
                .expect("GPU converter init");
            for (width, height, mode) in cases {
                let source = bgra_pattern(width, height);
                let texture = upload_texture(&context.device, width, height, &source);
                let transform = Transform::new(width, height, out_w, out_h, mode).unwrap();
                let mut expected = vec![0u8; out_w as usize * out_h as usize * 3 / 2];
                bgra_to_nv12(&source, width as usize * 4, &transform, &mut expected).unwrap();
                let mut actual = vec![0u8; expected.len()];
                assert!(
                    converter
                        .convert(&texture, &transform, &mut actual, None)
                        .expect("GPU conversion failed"),
                    "GPU conversion was unexpectedly cancelled"
                );
                if actual != expected {
                    let offset = actual
                        .iter()
                        .zip(&expected)
                        .position(|(a, e)| a != e)
                        .unwrap_or(0);
                    panic!(
                        "GPU/CPU mismatch for {width}x{height}->{out_w}x{out_h} {mode:?} at byte {offset}: gpu={} cpu={}",
                        actual[offset], expected[offset]
                    );
                }
            }
        }
    }
}
