# The integer test source stays canonical here; receiver and vcam also consume it.
add_library(vc-pattern INTERFACE)
target_include_directories(vc-pattern INTERFACE "${CMAKE_CURRENT_SOURCE_DIR}")

add_executable(vc-sender vc_sender.cpp "${PROJECT_SOURCE_DIR}/cmake/longpath.manifest")
vc_configure_target(vc-sender)
target_include_directories(vc-sender PRIVATE "${PROJECT_SOURCE_DIR}/proto")
target_link_libraries(vc-sender PRIVATE vc-common vc-pattern vc-npcap)
if(WIN32)
  target_link_libraries(vc-sender PRIVATE ws2_32 winmm d3d11 dxgi d3dcompiler)
endif()
vc_copy_to_build_root(vc-sender)
