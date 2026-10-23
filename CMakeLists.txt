cmake_minimum_required(VERSION 3.20)
project(VirtualCapture CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
# Root orchestration: sender + receiver (PLAN frozen CLI) + vcam DShow filter (P4-3).
add_subdirectory(sender)
add_subdirectory(receiver)
add_subdirectory(vcam)
