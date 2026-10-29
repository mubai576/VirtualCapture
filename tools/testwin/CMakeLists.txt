add_executable(vc-testwin WIN32 testwin.cpp)
vc_configure_target(vc-testwin)
target_link_libraries(vc-testwin PRIVATE d3d11 dxgi d3dcompiler)
