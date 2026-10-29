add_executable(vc-vcam-verify vcam_verify.cpp)
vc_configure_target(vc-vcam-verify)
target_link_libraries(vc-vcam-verify PRIVATE strmiids ole32 oleaut32)
# Preserve the existing build/vcam_verify/Release/vc-vcam-verify.exe path.
set_target_properties(vc-vcam-verify PROPERTIES
  RUNTIME_OUTPUT_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}")
