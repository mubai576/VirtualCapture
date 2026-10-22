cmake_minimum_required(VERSION 3.20)
# receiver/CMakeLists.txt — T-02 + P1-2. Same Npcap policy as sender.
add_executable(vc-receiver vc_receiver.cpp ${CMAKE_CURRENT_SOURCE_DIR}/../cmake/longpath.manifest)
target_include_directories(vc-receiver PRIVATE ${CMAKE_CURRENT_SOURCE_DIR} ${CMAKE_CURRENT_SOURCE_DIR}/../proto ${CMAKE_CURRENT_SOURCE_DIR}/../sender)
target_compile_features(vc-receiver PRIVATE cxx_std_17)
if(WIN32)
  target_link_libraries(vc-receiver PRIVATE ws2_32)
endif()
if(MSVC)
  target_compile_options(vc-receiver PRIVATE /utf-8)
endif()
# P1-2: NPCAP_SDK 未设时自动回退 ${CMAKE_SOURCE_DIR}/thirdparty/npcap-sdk，不依赖任何环境变量。
if(DEFINED ENV{NPCAP_SDK} AND EXISTS "$ENV{NPCAP_SDK}/Include/pcap.h")
  message(STATUS "receiver: using NPCAP_SDK from env $ENV{NPCAP_SDK}")
  target_include_directories(vc-receiver PRIVATE $ENV{NPCAP_SDK}/Include)
  target_compile_definitions(vc-receiver PRIVATE HAVE_NPCAP_SDK=1)
elseif(EXISTS "${CMAKE_SOURCE_DIR}/thirdparty/npcap-sdk/Include/pcap.h")
  message(STATUS "receiver: using fallback ${CMAKE_SOURCE_DIR}/thirdparty/npcap-sdk")
  target_include_directories(vc-receiver PRIVATE ${CMAKE_SOURCE_DIR}/thirdparty/npcap-sdk/Include)
  target_compile_definitions(vc-receiver PRIVATE HAVE_NPCAP_SDK=1)
else()
  message(STATUS "receiver: NPCAP_SDK not found (env + fallback missing), dynamic-load-only build (selftest OK)")
endif()
set_target_properties(vc-receiver PROPERTIES RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR})
# PLAN 1.2 product path is build/vc-receiver.exe (multi-config generators use Release/ subdir;
# copy up so acceptance `build/vc-receiver.exe --selftest` works regardless of generator).
add_custom_command(TARGET vc-receiver POST_BUILD COMMAND ${CMAKE_COMMAND} -E copy_if_different
  $<TARGET_FILE:vc-receiver> ${CMAKE_BINARY_DIR}/vc-receiver.exe)
