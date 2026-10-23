cmake_minimum_required(VERSION 3.20)
project(testwin CXX)
set(CMAKE_CXX_STANDARD 17)

add_executable(vc-testwin WIN32 testwin.cpp)
target_link_libraries(vc-testwin PRIVATE d3d11 dxgi d3dcompiler)
set_target_properties(vc-testwin PROPERTIES
    RUNTIME_OUTPUT_DIRECTORY_RELEASE "${CMAKE_SOURCE_DIR}/build/Release"
    RUNTIME_OUTPUT_DIRECTORY_DEBUG   "${CMAKE_SOURCE_DIR}/build/Debug"
)
