cmake_minimum_required(VERSION 3.20)
project(vc-vcam-verify CXX)
set(CMAKE_CXX_STANDARD 17)

add_executable(vc-vcam-verify vcam_verify.cpp)
target_link_libraries(vc-vcam-verify PRIVATE strmiids ole32 oleaut32)
