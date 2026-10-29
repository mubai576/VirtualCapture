add_executable(vc-monitor WIN32 monitor.cpp)
vc_configure_target(vc-monitor)
target_link_libraries(vc-monitor PRIVATE d3d11 dxgi d3dcompiler shlwapi)

add_executable(vc-monitor-diag diag.cpp)
vc_configure_target(vc-monitor-diag)
target_link_libraries(vc-monitor-diag PRIVATE d3d11 dxgi d3dcompiler user32)
