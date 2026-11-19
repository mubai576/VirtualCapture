# Scoped native runtime for the Rust portable package.
#
# This project intentionally does not include the legacy C++ product targets or
# install component.  It builds only what the Rust package loads at runtime:
#   * vc-vcam-verify.exe  (isolated, non-registration DShow capability probe)
#   * turbojpeg.dll       (pinned libjpeg-turbo 3.1.2 shared build)
#   * libpyrowave-shared-1.dll + licenses (pinned PyroWave build)
#   * app-local MSVC runtime and third-party notices
#
# The root CMakeLists.txt and cmake/Install.cmake are untouched, so the stable
# legacy build/package keeps producing its existing payload.
cmake_minimum_required(VERSION 3.20)
project(VirtualCaptureRustRuntime LANGUAGES C CXX)

if(NOT WIN32)
  message(FATAL_ERROR "The Rust portable native runtime is Windows-only")
endif()
if(NOT CMAKE_SIZEOF_VOID_P EQUAL 8)
  message(FATAL_ERROR "The Rust portable native runtime requires an x64 toolchain")
endif()
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

get_filename_component(VC_REPO_ROOT "${CMAKE_CURRENT_LIST_DIR}/../../.." ABSOLUTE)
include("${VC_REPO_ROOT}/cmake/VirtualCaptureTargets.cmake")
include("${VC_REPO_ROOT}/cmake/Mjpeg.cmake")
include("${VC_REPO_ROOT}/cmake/Pyrowave.cmake")

# ---------------------------------------------------------------------------
# Capability verifier
#
# `--dll <dll> --capabilities` only calls LoadLibraryW/GetProcAddress for the
# exported VCStreamCapabilities symbol.  It never calls DllRegisterServer or
# instantiates the filter, so it is safe to run against the Rust vc_vcam.dll.
# ---------------------------------------------------------------------------
add_executable(vc-vcam-verify "${VC_REPO_ROOT}/tools/vcam_verify/vcam_verify.cpp")
vc_configure_target(vc-vcam-verify)
set_property(TARGET vc-vcam-verify PROPERTY
  MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL")
target_link_libraries(vc-vcam-verify PRIVATE strmiids ole32 oleaut32)
install(TARGETS vc-vcam-verify RUNTIME DESTINATION . COMPONENT Runtime)

# ---------------------------------------------------------------------------
# Pinned TurboJPEG shared runtime
#
# cmake/Mjpeg.cmake builds the legacy static library.  The Rust codec loader
# wants a real turbojpeg.dll, so keep the same pinned source/NASM versions here
# but build ENABLE_SHARED=ON.  The pins are duplicated deliberately (a shared
# build cannot reuse vc_enable_mjpeg); the sanity check below fails if the
# legacy pins move without updating this file.
# ---------------------------------------------------------------------------
set(VC_TURBOJPEG_VERSION "3.1.2")
set(VC_TURBOJPEG_ARCHIVE_SHA256 "560f6338b547544c4f9721b18d8b87685d433ec78b3c644c70d77adad22c55e6")
set(VC_NASM_ARCHIVE_SHA256 "3ee4782247bcb874378d02f7eab4e294a84d3d15f3f6ee2de2f47a46aa7226e6")
file(READ "${VC_REPO_ROOT}/cmake/Mjpeg.cmake" VC_MJPEG_CMAKE)
foreach(pin IN ITEMS
    "libjpeg-turbo-${VC_TURBOJPEG_VERSION}"
    "${VC_TURBOJPEG_ARCHIVE_SHA256}"
    "${VC_NASM_ARCHIVE_SHA256}")
  string(FIND "${VC_MJPEG_CMAKE}" "${pin}" VC_PIN_INDEX)
  if(VC_PIN_INDEX EQUAL -1)
    message(FATAL_ERROR
      "cmake/Mjpeg.cmake pins changed; update rust/packaging/native/CMakeLists.txt")
  endif()
endforeach()

set(VC_DEPS "${CMAKE_BINARY_DIR}/_deps")
file(MAKE_DIRECTORY "${VC_DEPS}/downloads")
set(VC_TURBOJPEG_ARCHIVE
  "${VC_DEPS}/downloads/libjpeg-turbo-${VC_TURBOJPEG_VERSION}.tar.gz")
vc_download_checked(
  "https://codeload.github.com/libjpeg-turbo/libjpeg-turbo/tar.gz/refs/tags/${VC_TURBOJPEG_VERSION}"
  "${VC_TURBOJPEG_ARCHIVE}" "${VC_TURBOJPEG_ARCHIVE_SHA256}")
set(VC_TURBOJPEG_SOURCE "${VC_DEPS}/libjpeg-turbo-${VC_TURBOJPEG_VERSION}")
if(NOT EXISTS "${VC_TURBOJPEG_SOURCE}/CMakeLists.txt")
  file(ARCHIVE_EXTRACT INPUT "${VC_TURBOJPEG_ARCHIVE}" DESTINATION "${VC_DEPS}")
endif()

set(VC_NASM_ARCHIVE "${VC_DEPS}/downloads/nasm-2.16.03-win64.zip")
vc_download_checked(
  "https://www.nasm.us/pub/nasm/releasebuilds/2.16.03/win64/nasm-2.16.03-win64.zip"
  "${VC_NASM_ARCHIVE}" "${VC_NASM_ARCHIVE_SHA256}")
if(NOT EXISTS "${VC_DEPS}/nasm-2.16.03/nasm.exe")
  file(ARCHIVE_EXTRACT INPUT "${VC_NASM_ARCHIVE}" DESTINATION "${VC_DEPS}")
endif()

set(VC_TURBOJPEG_SHARED_BINARY "${VC_DEPS}/libjpeg-turbo-shared")
set(VC_GENERATOR_ARGS CMAKE_GENERATOR "${CMAKE_GENERATOR}")
if(CMAKE_GENERATOR_PLATFORM)
  list(APPEND VC_GENERATOR_ARGS CMAKE_GENERATOR_PLATFORM "${CMAKE_GENERATOR_PLATFORM}")
endif()
if(CMAKE_GENERATOR_TOOLSET)
  list(APPEND VC_GENERATOR_ARGS CMAKE_GENERATOR_TOOLSET "${CMAKE_GENERATOR_TOOLSET}")
endif()
ExternalProject_Add(vc-turbojpeg-shared-build
  SOURCE_DIR "${VC_TURBOJPEG_SOURCE}"
  BINARY_DIR "${VC_TURBOJPEG_SHARED_BINARY}"
  DOWNLOAD_COMMAND "" UPDATE_COMMAND ""
  ${VC_GENERATOR_ARGS}
  CMAKE_ARGS -DENABLE_SHARED=ON -DENABLE_STATIC=OFF -DWITH_TURBOJPEG=ON
    -DWITH_TOOLS=OFF -DWITH_TESTS=OFF -DWITH_JAVA=OFF
    -DWITH_SIMD=ON -DREQUIRE_SIMD=ON -DWITH_CRT_DLL=ON
    "-DCMAKE_ASM_NASM_COMPILER=${VC_DEPS}/nasm-2.16.03/nasm.exe"
    "-DCMAKE_BUILD_TYPE=${CMAKE_BUILD_TYPE}" "-DCMAKE_C_FLAGS=/utf-8"
  BUILD_COMMAND "${CMAKE_COMMAND}" --build <BINARY_DIR> --config $<CONFIG> --target turbojpeg
  INSTALL_COMMAND "")
foreach(config Debug Release RelWithDebInfo MinSizeRel)
  install(FILES "${VC_TURBOJPEG_SHARED_BINARY}/${config}/turbojpeg.dll"
    DESTINATION . COMPONENT Runtime CONFIGURATIONS ${config})
endforeach()
install(FILES "${VC_TURBOJPEG_SOURCE}/LICENSE.md"
  DESTINATION licenses RENAME libjpeg-turbo-LICENSE.md COMPONENT Runtime)
install(FILES "${VC_TURBOJPEG_SOURCE}/README.ijg"
  DESTINATION licenses RENAME libjpeg-turbo-README.ijg COMPONENT Runtime)

# ---------------------------------------------------------------------------
# Pinned PyroWave shared runtime
#
# Reuse the repository's pin/fetch/build function.  The probe target exists
# only because vc_enable_pyrowave() expects a link target; it is excluded from
# ALL and never linked into a package binary.  vc_enable_pyrowave() installs
# libpyrowave-shared-1.dll plus the PyroWave/Granite notices into "Runtime".
# ---------------------------------------------------------------------------
add_library(vc-native-pyrowave-probe STATIC "${CMAKE_CURRENT_LIST_DIR}/probe.cpp")
set_property(TARGET vc-native-pyrowave-probe PROPERTY EXCLUDE_FROM_ALL TRUE)
vc_enable_pyrowave(vc-native-pyrowave-probe)

set(VC_PYROWAVE_SOURCE "${VC_DEPS}/pyrowave-src")
install(FILES "${VC_PYROWAVE_SOURCE}/Granite/third_party/volk/LICENSE.md"
  DESTINATION licenses RENAME volk-LICENSE.md COMPONENT Runtime)
install(FILES "${VC_PYROWAVE_SOURCE}/Granite/third_party/khronos/vulkan-headers/LICENSE.md"
  DESTINATION licenses RENAME Vulkan-Headers-LICENSE.md COMPONENT Runtime)
install(DIRECTORY "${VC_PYROWAVE_SOURCE}/Granite/third_party/khronos/vulkan-headers/LICENSES/"
  DESTINATION licenses/Vulkan-Headers-LICENSES COMPONENT Runtime)

# ---------------------------------------------------------------------------
# App-local MSVC runtime.  The package smoke test checks that the GUI/capture
# processes load these copies rather than a system installation.
# ---------------------------------------------------------------------------
set(CMAKE_INSTALL_SYSTEM_RUNTIME_DESTINATION .)
set(CMAKE_INSTALL_SYSTEM_RUNTIME_COMPONENT Runtime)
include(InstallRequiredSystemLibraries)
