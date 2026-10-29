cmake_minimum_required(VERSION 3.20)
project(VirtualCapture CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
include(cmake/VirtualCaptureTargets.cmake)
include(cmake/Npcap.cmake)

# Shared algorithms, then product components, then local development tools.
add_subdirectory(common)
add_subdirectory(sender)
add_subdirectory(receiver)
add_subdirectory(vcam)
add_subdirectory(tools)
include(cmake/Install.cmake)
