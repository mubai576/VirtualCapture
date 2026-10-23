cmake_minimum_required(VERSION 3.20)
project(vc-monitor CXX)
set(CMAKE_CXX_STANDARD 17)

add_executable(vc-monitor WIN32 monitor.cpp)
target_link_libraries(vc-monitor PRIVATE d3d11 dxgi d3dcompiler shlwapi)
set_target_properties(vc-monitor PROPERTIES
    RUNTIME_OUTPUT_DIRECTORY_RELEASE "${CMAKE_SOURCE_DIR}/build/Release"
    RUNTIME_OUTPUT_DIRECTORY_DEBUG   "${CMAKE_SOURCE_DIR}/build/Debug"
)

add_executable(vc-monitor-diag diag.cpp)
target_link_libraries(vc-monitor-diag PRIVATE d3d11 dxgi d3dcompiler user32)
set_target_properties(vc-monitor-diag PROPERTIES
    RUNTIME_OUTPUT_DIRECTORY_RELEASE "${CMAKE_SOURCE_DIR}/build/Release"
)
