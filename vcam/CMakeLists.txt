# P4-3 DShow source filter: user-mode COM, no ATL or strmbase dependency.
add_library(vc-vcam SHARED vcam.cpp vcam.def)
vc_configure_target(vc-vcam)
target_link_libraries(vc-vcam PRIVATE vc-pattern)
target_compile_definitions(vc-vcam PRIVATE UNICODE _UNICODE)
if(WIN32)
  target_link_libraries(vc-vcam PRIVATE strmiids winmm)
endif()
# No post-build copy: registration uses build/Release/vc-vcam.dll directly.
