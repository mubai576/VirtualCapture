"""Single source of truth for the Rust-R3 portable package payload.

Both ``package.py`` and the extracted-payload smoke test import this module
explicitly (the scripts run under ``python -I``, so the script directory is not
on ``sys.path``).  Keep this file free of imports from the rest of the build.
"""

# Product entry and Rust programs.  ``vc-launcher.exe`` is renamed to
# ``VirtualCapture.exe`` when staged; the two diagnostic EXEs are first-class
# payload files, not build leftovers.
LAUNCHERS = ("VirtualCapture.exe",)
RUST_EXES = (
    "vc-host.exe",
    "vc-capture.exe",
    "vc-updater.exe",
    "vc-stream.exe",
    "vc-transport.exe",
)
RUST_DLLS = (
    "vc_stream.dll",  # in-process capture/transport module (versioned C ABI)
    "vc_vcam.dll",    # Rust DirectShow source filter (COM/DShow ABI)
)
NATIVE_RUNTIME = (
    "vc-vcam-verify.exe",          # isolated read-only capability probe
    "turbojpeg.dll",               # pinned libjpeg-turbo 3.1.2 shared build
    "libpyrowave-shared-1.dll",    # pinned PyroWave shared build
)

# Capability contract shared with vc-core::process::validate_capabilities and
# the packaged vc_stream.dll.  Raw is the default; encoded codecs must remain
# explicitly selected and never fall back automatically.
STREAM_ABI = 1
CODECS = ("raw", "mjpeg", "pyrowave")
SOURCES = ("pattern", "dda", "wgc")
CODEC_DLLS = {
    "mjpeg": "turbojpeg.dll",
    "pyrowave": "libpyrowave-shared-1.dll",
}
MSVC_RUNTIME = (
    "msvcp140.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
)
REQUIRED_FILES = LAUNCHERS + RUST_EXES + RUST_DLLS + NATIVE_RUNTIME + MSVC_RUNTIME

# These are the C++ production binaries from the legacy package.  The Rust
# package must not smuggle them in as a hidden fallback data plane.
FORBIDDEN_NAMES = frozenset(name.lower() for name in (
    "vc-sender.exe",
    "vc-receiver.exe",
    "vc-vcam.dll",
    "vc-monitor.exe",
    "vc-monitor-diag.exe",
    "vc-testwin.exe",
    "vc-vcam-lifecycle.exe",
))

# Native dependency notices installed by rust/packaging/native/CMakeLists.txt,
# plus the generated MSVC runtime notice.
REQUIRED_LICENSES = (
    "libjpeg-turbo-LICENSE.md",
    "libjpeg-turbo-README.ijg",
    "pyrowave-LICENSE.txt",
    "granite-LICENSE.txt",
    "volk-LICENSE.md",
    "Vulkan-Headers-LICENSE.md",
    "Vulkan-Headers-LICENSES/Apache-2.0.txt",
    "Vulkan-Headers-LICENSES/MIT.txt",
    "MSVC-runtime.txt",
)

# Produced by rust/packaging/collect_licenses.py.
RUST_LICENSE_OUTPUTS = (
    "rust/inventory.json",
    "rust/THIRD-PARTY-NOTICES.md",
)

# Every top-level PE in the release directory must be one of these.  This keeps
# diagnostics from growing accidentally and catches legacy binaries even when
# their names are not in FORBIDDEN_NAMES.
EXPECTED_PE_NAMES = frozenset(
    name.lower() for name in REQUIRED_FILES
)
