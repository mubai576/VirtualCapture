"""Rust 便携包的唯一载荷清单，构建器与解压验证共用。"""

LAUNCHERS = ("VirtualCapture.exe",)
RUST_EXES = (
    "vc-host.exe", "vc-capture.exe", "vc-updater.exe",
    "vc-stream.exe", "vc-transport.exe", "vc-vcam-verify.exe",
)
RUST_DLLS = ("vc_stream.dll", "vc_vcam.dll")
# 上游 PyroWave 为 Vulkan 第三方运行库，不含本项目 C++ 产品实现。
NATIVE_RUNTIME = ("libpyrowave-shared-1.dll",)
STREAM_ABI = 1
CODECS = ("raw", "pyrowave")
SOURCES = ("pattern", "dda", "wgc")
CODEC_DLLS = {"pyrowave": "libpyrowave-shared-1.dll"}
MSVC_RUNTIME = ("msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll")
REQUIRED_FILES = LAUNCHERS + RUST_EXES + RUST_DLLS + NATIVE_RUNTIME + MSVC_RUNTIME

# 禁止旧产品或 CPU 编码依赖混入并形成隐藏回退。
FORBIDDEN_NAMES = frozenset(name.lower() for name in (
    "vc-sender.exe", "vc-receiver.exe", "vc-vcam.dll", "vc-monitor.exe",
    "vc-monitor-diag.exe", "vc-testwin.exe", "vc-vcam-lifecycle.exe", "turbojpeg.dll",
))
REQUIRED_LICENSES = (
    "pyrowave-LICENSE.txt", "granite-LICENSE.txt", "volk-LICENSE.md",
    "Vulkan-Headers-LICENSE.md", "Vulkan-Headers-LICENSES/Apache-2.0.txt",
    "Vulkan-Headers-LICENSES/MIT.txt", "MSVC-runtime.txt",
)
RUST_LICENSE_OUTPUTS = ("rust/inventory.json", "rust/THIRD-PARTY-NOTICES.md")
EXPECTED_PE_NAMES = frozenset(name.lower() for name in REQUIRED_FILES)
