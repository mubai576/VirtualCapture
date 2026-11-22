"""Rust 便携包的唯一载荷清单，构建器与解压验证共用。"""

import re

MANIFEST_NAME = "package-manifest.json"
ENTRYPOINT = "unified-v1"
APP_EXE = "VirtualCapture.exe"
UPDATER_EXE = "vc-updater.exe"
RUST_EXES = (APP_EXE, UPDATER_EXE)
RUST_DLLS = ("vc_stream.dll", "vc_vcam.dll")
VCAM_PROBE_ARGS = ("__vcam-probe", "--dll")
VCAM_PROBE_COMMAND = (APP_EXE, *VCAM_PROBE_ARGS, "<absolute path>")
# 上游 PyroWave 为 Vulkan 第三方运行库，不含本项目 C++ 产品实现。
NATIVE_RUNTIME = ("libpyrowave-shared-1.dll",)
STREAM_ABI = 1
CODECS = ("raw", "pyrowave")
SOURCES = ("pattern", "dda", "wgc")
CODEC_DLLS = {"pyrowave": "libpyrowave-shared-1.dll"}
MSVC_RUNTIME = ("msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll")
REQUIRED_PE_FILES = RUST_EXES + RUST_DLLS + NATIVE_RUNTIME + MSVC_RUNTIME
EXPECTED_PE_NAMES = frozenset(name.lower() for name in REQUIRED_PE_FILES)
REQUIRED_FILES = REQUIRED_PE_FILES + ("README.md", "version.json")
MSVC_RUNTIME_DLL_PATTERN = re.compile(r"(?:msvcp|vcruntime|concrt|vcomp|ucrtbase)[a-z0-9_.-]*\.dll")
NATIVE_LICENSE_AUDIT_FILES = (
    "pyrowave-LICENSE.txt", "granite-LICENSE.txt", "volk-LICENSE.md",
    "Vulkan-Headers-LICENSE.md", "Vulkan-Headers-LICENSES/Apache-2.0.txt",
    "Vulkan-Headers-LICENSES/MIT.txt", "MSVC-runtime.txt",
)
RUST_LICENSE_AUDIT_OUTPUTS = ("inventory.json", "THIRD-PARTY-NOTICES.md")


def is_msvc_runtime_dll(name):
    return MSVC_RUNTIME_DLL_PATTERN.fullmatch(name.lower()) is not None


def validate_payload_paths(paths, *, with_manifest=False):
    """All payload members are flat; only MSVC DLL names may extend the allowlist."""
    required = set(REQUIRED_FILES)
    if with_manifest:
        required.add(MANIFEST_NAME)
    present = set()
    seen = set()
    for path in paths:
        key = path.lower()
        if key in seen:
            raise RuntimeError(f"Duplicate Windows payload path: {path}")
        if path not in required and not is_msvc_runtime_dll(path):
            raise RuntimeError(f"Unexpected payload path: {path}")
        present.add(path)
        seen.add(key)
    missing = sorted(required - present)
    if missing:
        raise RuntimeError(f"Missing payload files: {missing}")
    return present


def validate_payload_directory(directory, *, with_manifest=False):
    entries = list(directory.iterdir())
    non_files = sorted(entry.name for entry in entries if entry.is_symlink() or not entry.is_file())
    if non_files:
        raise RuntimeError(f"Unexpected directory or non-regular payload entry: {non_files}")
    return validate_payload_paths((entry.name for entry in entries), with_manifest=with_manifest)
