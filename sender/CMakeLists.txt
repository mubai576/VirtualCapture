cmake_minimum_required(VERSION 3.20)
# sender/CMakeLists.txt — T-01 + P1-2. C++17, Npcap headers via NPCAP_SDK env or project fallback.
# Live code uses dynamic wpcap load, so build succeeds even without SDK (selftest needs no NIC).
add_executable(vc-sender vc_sender.cpp ${CMAKE_CURRENT_SOURCE_DIR}/../cmake/longpath.manifest)
target_include_directories(vc-sender PRIVATE ${CMAKE_CURRENT_SOURCE_DIR} ${CMAKE_CURRENT_SOURCE_DIR}/../proto)
target_compile_features(vc-sender PRIVATE cxx_std_17)
if(WIN32)
  target_link_libraries(vc-sender PRIVATE ws2_32 winmm d3d11 dxgi d3dcompiler)
endif()
if(MSVC)
  target_compile_options(vc-sender PRIVATE /utf-8)
endif()
# P1-2: NPCAP_SDK 未设时自动回退 ${CMAKE_SOURCE_DIR}/thirdparty/npcap-sdk，不依赖任何环境变量。
if(DEFINED ENV{NPCAP_SDK} AND EXISTS "$ENV{NPCAP_SDK}/Include/pcap.h")
  message(STATUS "sender: using NPCAP_SDK from env $ENV{NPCAP_SDK}")
  target_include_directories(vc-sender PRIVATE $ENV{NPCAP_SDK}/Include)
  target_compile_definitions(vc-sender PRIVATE HAVE_NPCAP_SDK=1)
elseif(EXISTS "${CMAKE_SOURCE_DIR}/thirdparty/npcap-sdk/Include/pcap.h")
  message(STATUS "sender: using fallback ${CMAKE_SOURCE_DIR}/thirdparty/npcap-sdk")
  target_include_directories(vc-sender PRIVATE ${CMAKE_SOURCE_DIR}/thirdparty/npcap-sdk/Include)
  target_compile_definitions(vc-sender PRIVATE HAVE_NPCAP_SDK=1)
else()
  message(STATUS "sender: NPCAP_SDK not found (env + fallback missing), dynamic-load-only build (selftest OK)")
endif()
set_target_properties(vc-sender PROPERTIES RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR})
# PLAN 1.2 product path is build/vc-sender.exe (multi-config generators use Release/ subdir;
# copy up so acceptance `build/vc-sender.exe --selftest` works regardless of generator).
add_custom_command(TARGET vc-sender POST_BUILD COMMAND ${CMAKE_COMMAND} -E copy_if_different
  $<TARGET_FILE:vc-sender> ${CMAKE_BINARY_DIR}/vc-sender.exe)
