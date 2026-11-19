# 仅构建 Rust 程序加载的第三方 Vulkan 运行库；产品和隔离验证器全部由 Cargo 构建。
cmake_minimum_required(VERSION 3.20)
project(VirtualCaptureNativeDependencies LANGUAGES C CXX)

if(NOT WIN32 OR NOT CMAKE_SIZEOF_VOID_P EQUAL 8)
  message(FATAL_ERROR "VirtualCapture requires a Windows x64 toolchain")
endif()
include("${CMAKE_CURRENT_LIST_DIR}/Pyrowave.cmake")
vc_build_pyrowave()

# 从本机 Visual Studio 可再分发目录安装程序本地运行库，不依赖目标机安装开发工具。
set(CMAKE_INSTALL_SYSTEM_RUNTIME_DESTINATION .)
set(CMAKE_INSTALL_SYSTEM_RUNTIME_COMPONENT Runtime)
include(InstallRequiredSystemLibraries)
