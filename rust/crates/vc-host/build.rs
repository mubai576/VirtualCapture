//! 从文本 SVG 生成多尺寸 Windows 图标并作为 exe 资源 1 嵌入。
//!
//! GPUI 的 Windows 后端用 `LoadImageW(module, id=1)` 读取窗口类图标，因此
//! 这里只需把图标编译进 exe，标题栏与任务栏都会使用它。生成物只写 OUT_DIR，
//! 不把二进制图标提交到源码树。没有新增 crate 依赖：使用受限形状解析、
//! 4x 超采样光栅化、无压缩 deflate 的 PNG 帧和手写 ICO 容器。

use std::path::{Path, PathBuf};
use std::process::Command;

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

#[derive(Clone)]
struct Shape {
    color: Rgba,
    kind: Kind,
}

#[derive(Clone)]
enum Kind {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
    },
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        width: f32,
    },
    Polygon(Vec<(f32, f32)>),
    Circle {
        cx: f32,
        cy: f32,
        r: f32,
    },
}

#[derive(Clone, Copy)]
struct Rgba {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.svg");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let svg = std::fs::read_to_string("assets/app-icon.svg").expect("读取 assets/app-icon.svg");
    let shapes = parse_svg(&svg);
    let ico = build_ico(&shapes);
    let ico_path = out.join("vc-host.ico");
    std::fs::write(&ico_path, ico).expect("写入生成的 ICO");
    let rc_path = out.join("vc-host.rc");
    std::fs::write(
        &rc_path,
        format!(
            "1 ICON \"{}\"\n",
            ico_path.display().to_string().replace('\\', "/")
        ),
    )
    .expect("写入资源脚本");
    let res_path = out.join("vc-host.res");
    let rc = find_rc()
        .unwrap_or_else(|| panic!("找不到 rc.exe；请安装 Windows SDK 或设置 VC_RC 指向 rc.exe"));
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", res_path.display()))
        .arg(&rc_path)
        .status()
        .expect("运行 rc.exe");
    assert!(status.success(), "rc.exe 编译图标资源失败");
    println!("cargo:rustc-link-arg={}", res_path.display());
}

/// 只解析本仓库图标使用的形状子集；不引入 XML/SVG 依赖。
fn parse_svg(text: &str) -> Vec<Shape> {
    let mut shapes = Vec::new();
    let mut stroke = Rgba {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };
    let mut stroke_width = 0.0;
    let mut index = 0;
    while let Some(start) = text[index..].find('<') {
        let start = index + start;
        let Some(end_rel) = text[start..].find('>') else {
            break;
        };
        let end = start + end_rel;
        let tag = text[start + 1..end].trim_end_matches('/').trim();
        index = end + 1;
        if tag.starts_with("g ") {
            stroke = attr_color(tag, "stroke").unwrap_or(stroke);
            stroke_width = attr_num(tag, "stroke-width").unwrap_or(stroke_width);
        } else if let Some(body) = tag.strip_prefix("rect ") {
            shapes.push(Shape {
                color: attr_color(body, "fill").unwrap_or(Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                }),
                kind: Kind::Rect {
                    x: attr_num(body, "x").unwrap_or(0.0),
                    y: attr_num(body, "y").unwrap_or(0.0),
                    w: attr_num(body, "width").unwrap_or(0.0),
                    h: attr_num(body, "height").unwrap_or(0.0),
                    r: attr_num(body, "rx").unwrap_or(0.0),
                },
            });
        } else if let Some(body) = tag.strip_prefix("line ") {
            shapes.push(Shape {
                color: stroke,
                kind: Kind::Line {
                    x1: attr_num(body, "x1").unwrap_or(0.0),
                    y1: attr_num(body, "y1").unwrap_or(0.0),
                    x2: attr_num(body, "x2").unwrap_or(0.0),
                    y2: attr_num(body, "y2").unwrap_or(0.0),
                    width: stroke_width,
                },
            });
        } else if let Some(body) = tag.strip_prefix("polygon ") {
            let points = attr(body, "points")
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|pair| pair.split_once(','))
                .filter_map(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?)))
                .collect();
            shapes.push(Shape {
                color: attr_color(body, "fill").unwrap_or(Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                }),
                kind: Kind::Polygon(points),
            });
        } else if let Some(body) = tag.strip_prefix("circle ") {
            shapes.push(Shape {
                color: attr_color(body, "fill").unwrap_or(Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                }),
                kind: Kind::Circle {
                    cx: attr_num(body, "cx").unwrap_or(0.0),
                    cy: attr_num(body, "cy").unwrap_or(0.0),
                    r: attr_num(body, "r").unwrap_or(0.0),
                },
            });
        }
    }
    shapes
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

fn attr_num(tag: &str, name: &str) -> Option<f32> {
    attr(tag, name)?.parse().ok()
}

fn attr_color(tag: &str, name: &str) -> Option<Rgba> {
    let value = attr(tag, name)?;
    if value.eq_ignore_ascii_case("none") {
        return None;
    }
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    Some(Rgba {
        r: u8::from_str_radix(&hex[0..2], 16).ok()?,
        g: u8::from_str_radix(&hex[2..4], 16).ok()?,
        b: u8::from_str_radix(&hex[4..6], 16).ok()?,
        a: 255,
    })
}

fn build_ico(shapes: &[Shape]) -> Vec<u8> {
    let frames: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&size| {
            let rgba = render(shapes, size);
            // 256px 使用 PNG，符合现代 shell 的预期；更小的帧保持 DIB，
            // 所有 Windows 图标加载器都能接受。
            let data = if size >= 256 {
                encode_png(size, &rgba)
            } else {
                encode_dib(size, &rgba)
            };
            (size, data)
        })
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0, frames.len() as u8, 0]);
    let mut offset = 6 + 16 * frames.len() as u32;
    for (size, data) in &frames {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in frames {
        out.extend_from_slice(&data);
    }
    out
}

fn render(shapes: &[Shape], size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let big = size * SS;
    let scale = big as f32 / 1024.0;
    let mut samples = vec![[0f32; 4]; (big * big) as usize];
    for shape in shapes {
        draw(shape, scale, big, &mut samples);
    }
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut sum = [0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = samples[((y * SS + sy) * big + x * SS + sx) as usize];
                    let a = px[3];
                    sum[0] += px[0] * a;
                    sum[1] += px[1] * a;
                    sum[2] += px[2] * a;
                    sum[3] += a;
                }
            }
            let alpha = sum[3] / (SS * SS) as f32;
            let (r, g, b) = if sum[3] > 0.0 {
                (
                    ((sum[0] / sum[3]) * 255.0) as u8,
                    ((sum[1] / sum[3]) * 255.0) as u8,
                    ((sum[2] / sum[3]) * 255.0) as u8,
                )
            } else {
                (0, 0, 0)
            };
            out.extend_from_slice(&[r, g, b, (alpha * 255.0).round() as u8]);
        }
    }
    out
}

fn draw(shape: &Shape, scale: f32, dim: u32, out: &mut [[f32; 4]]) {
    let c = shape.color;
    let rgb = [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0];
    let put = |x: u32, y: u32, out: &mut [[f32; 4]]| {
        if x < dim && y < dim {
            let slot = &mut out[(y * dim + x) as usize];
            let a = c.a as f32 / 255.0;
            let inv = 1.0 - a;
            *slot = [
                rgb[0] * a + slot[0] * inv,
                rgb[1] * a + slot[1] * inv,
                rgb[2] * a + slot[2] * inv,
                a + slot[3] * inv,
            ];
        }
    };
    match &shape.kind {
        Kind::Rect { x, y, w, h, r } => {
            let (x, y, w, h, r) = (x * scale, y * scale, w * scale, h * scale, r * scale);
            let x0 = x.floor().max(0.0) as u32;
            let y0 = y.floor().max(0.0) as u32;
            let x1 = (x + w).ceil().min(dim as f32) as u32;
            let y1 = (y + h).ceil().min(dim as f32) as u32;
            for py in y0..y1 {
                for px in x0..x1 {
                    let cx = px as f32 + 0.5;
                    let cy = py as f32 + 0.5;
                    if inside_round_rect(cx, cy, x, y, w, h, r) {
                        put(px, py, out);
                    }
                }
            }
        }
        Kind::Line {
            x1,
            y1,
            x2,
            y2,
            width,
        } => {
            let (x1, y1, x2, y2) = (x1 * scale, y1 * scale, x2 * scale, y2 * scale);
            let half = width * scale / 2.0;
            let x0 = (x1.min(x2) - half).floor().max(0.0) as u32;
            let y0 = (y1.min(y2) - half).floor().max(0.0) as u32;
            let x3 = (x1.max(x2) + half).ceil().min(dim as f32) as u32;
            let y3 = (y1.max(y2) + half).ceil().min(dim as f32) as u32;
            for py in y0..y3 {
                for px in x0..x3 {
                    if seg_dist(px as f32 + 0.5, py as f32 + 0.5, x1, y1, x2, y2) <= half {
                        put(px, py, out);
                    }
                }
            }
        }
        Kind::Polygon(points) => {
            if points.len() < 3 {
                return;
            }
            let scaled: Vec<(f32, f32)> =
                points.iter().map(|(x, y)| (x * scale, y * scale)).collect();
            let min_x = scaled
                .iter()
                .map(|p| p.0)
                .fold(f32::MAX, f32::min)
                .floor()
                .max(0.0) as u32;
            let min_y = scaled
                .iter()
                .map(|p| p.1)
                .fold(f32::MAX, f32::min)
                .floor()
                .max(0.0) as u32;
            let max_x = scaled
                .iter()
                .map(|p| p.0)
                .fold(f32::MIN, f32::max)
                .ceil()
                .min(dim as f32) as u32;
            let max_y = scaled
                .iter()
                .map(|p| p.1)
                .fold(f32::MIN, f32::max)
                .ceil()
                .min(dim as f32) as u32;
            for py in min_y..max_y {
                for px in min_x..max_x {
                    if point_in_poly(px as f32 + 0.5, py as f32 + 0.5, &scaled) {
                        put(px, py, out);
                    }
                }
            }
        }
        Kind::Circle { cx, cy, r } => {
            let (cx, cy, r) = (cx * scale, cy * scale, r * scale);
            let x0 = (cx - r).floor().max(0.0) as u32;
            let y0 = (cy - r).floor().max(0.0) as u32;
            let x1 = (cx + r).ceil().min(dim as f32) as u32;
            let y1 = (cy + r).ceil().min(dim as f32) as u32;
            for py in y0..y1 {
                for px in x0..x1 {
                    let dx = px as f32 + 0.5 - cx;
                    let dy = py as f32 + 0.5 - cy;
                    if dx * dx + dy * dy <= r * r {
                        put(px, py, out);
                    }
                }
            }
        }
    }
}

fn inside_round_rect(x: f32, y: f32, rx: f32, ry: f32, w: f32, h: f32, r: f32) -> bool {
    if x < rx || y < ry || x > rx + w || y > ry + h {
        return false;
    }
    let r = r.min(w / 2.0).min(h / 2.0);
    let cx = x.clamp(rx + r, rx + w - r);
    let cy = y.clamp(ry + r, ry + h - r);
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= r * r
}

fn seg_dist(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((px - x1) * dx + (py - y1) * dy) / len2).clamp(0.0, 1.0)
    };
    let ex = px - (x1 + t * dx);
    let ey = py - (y1 + t * dy);
    (ex * ex + ey * ey).sqrt()
}

fn point_in_poly(x: f32, y: f32, points: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = points.len() - 1;
    for i in 0..points.len() {
        let (xi, yi) = points[i];
        let (xj, yj) = points[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn encode_dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4 + size * size / 8) as usize + 40);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(size * size * 4).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    // XOR 位图：BGRA，自底向上。
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    // AND 掩码：零表示“使用 alpha 通道”。
    let stride = size.div_ceil(32) * 4;
    out.resize(out.len() + (stride * size) as usize, 0);
    out
}

fn encode_png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((size * (size * 4 + 1)) as usize);
    for y in 0..size {
        raw.push(0);
        let start = (y * size * 4) as usize;
        raw.extend_from_slice(&rgba[start..start + (size * 4) as usize]);
    }
    let mut zlib = vec![0x78, 0x01];
    let chunks = raw.chunks(65535);
    let mut adler = (1u32, 0u32);
    for &byte in &raw {
        adler.0 = (adler.0 + byte as u32) % 65521;
        adler.1 = (adler.1 + adler.0) % 65521;
    }
    let count = raw.len().div_ceil(65535);
    for (block, chunk) in chunks.enumerate() {
        let last = block + 1 == count;
        zlib.push(if last { 1 } else { 0 });
        let len = chunk.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(chunk);
    }
    zlib.extend_from_slice(&((adler.1 << 16) | adler.0).to_be_bytes());
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn find_rc() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("VC_RC") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "x64",
    };
    let roots = [
        std::env::var("WindowsSdkDir").ok().map(PathBuf::from),
        std::env::var("ProgramFiles(x86)")
            .ok()
            .map(|p| Path::new(&p).join("Windows Kits/10")),
        Some(PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10")),
    ];
    for root in roots.into_iter().flatten() {
        let bin = root.join("bin");
        let Ok(entries) = std::fs::read_dir(&bin) else {
            continue;
        };
        let mut versions: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.is_dir())
            .collect();
        versions.sort();
        for version in versions.into_iter().rev() {
            let candidate = version.join(arch).join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
