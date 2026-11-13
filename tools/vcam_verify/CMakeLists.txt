add_executable(vc-vcam-verify vcam_verify.cpp)
vc_configure_target(vc-vcam-verify)
target_link_libraries(vc-vcam-verify PRIVATE strmiids ole32 oleaut32)
# Preserve the existing build/vcam_verify/Release/vc-vcam-verify.exe path.
set_target_properties(vc-vcam-verify PROPERTIES
  RUNTIME_OUTPUT_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}")
add_executable(vc-vcam-lifecycle vcam_lifecycle.cpp)
vc_configure_target(vc-vcam-lifecycle)
target_link_libraries(vc-vcam-lifecycle PRIVATE strmiids ole32 psapi)
set_target_properties(vc-vcam-lifecycle PROPERTIES RUNTIME_OUTPUT_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}")
