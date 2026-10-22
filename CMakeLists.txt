cmake_minimum_required(VERSION 3.20)
project(VirtualCapture CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
# Root orchestration per PLAN §2.1 → build/vc-sender.exe + build/vc-receiver.exe
add_subdirectory(sender)
add_subdirectory(receiver)
