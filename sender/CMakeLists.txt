# The integer test source stays canonical here; receiver and vcam also consume it.
add_library(vc-pattern INTERFACE)
target_include_directories(vc-pattern INTERFACE "${CMAKE_CURRENT_SOURCE_DIR}")

add_executable(vc-sender vc_sender.cpp "${PROJECT_SOURCE_DIR}/cmake/longpath.manifest")
vc_configure_target(vc-sender)
target_include_directories(vc-sender PRIVATE "${PROJECT_SOURCE_DIR}/proto")
target_link_libraries(vc-sender PRIVATE vc-common vc-pattern vc-npcap)
if(WIN32)
  include("${PROJECT_SOURCE_DIR}/cmake/Mjpeg.cmake")
  vc_enable_mjpeg(vc-sender)
  include("${PROJECT_SOURCE_DIR}/cmake/Pyrowave.cmake")
  vc_enable_pyrowave(vc-sender)
  # C++/WinRT headers ship inside the Windows SDK; the VS generator does not
  # always put cppwinrt on INCLUDE, so resolve it from the target platform version.
  find_path(VC_CPPWINRT_INCLUDE winrt/base.h
    HINTS "$ENV{WindowsSdkDir}Include/${CMAKE_VS_WINDOWS_TARGET_PLATFORM_VERSION}/cppwinrt"
    PATHS "C:/Program Files (x86)/Windows Kits/10/Include/${CMAKE_VS_WINDOWS_TARGET_PLATFORM_VERSION}/cppwinrt"
    NO_DEFAULT_PATH)
  if(NOT VC_CPPWINRT_INCLUDE)
    file(GLOB _vc_cppwinrt_dirs "C:/Program Files (x86)/Windows Kits/10/Include/*/cppwinrt")
    list(SORT _vc_cppwinrt_dirs)
    if(_vc_cppwinrt_dirs)
      list(GET _vc_cppwinrt_dirs -1 VC_CPPWINRT_INCLUDE)
    endif()
  endif()
  if(VC_CPPWINRT_INCLUDE)
    target_include_directories(vc-sender PRIVATE "${VC_CPPWINRT_INCLUDE}")
  endif()
  target_link_libraries(vc-sender PRIVATE ws2_32 winmm d3d11 dxgi d3dcompiler ole32 oleaut32 windowsapp dwmapi)
endif()
vc_copy_to_build_root(vc-sender)
