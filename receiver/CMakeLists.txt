add_executable(vc-receiver vc_receiver.cpp "${PROJECT_SOURCE_DIR}/cmake/longpath.manifest")
vc_configure_target(vc-receiver)
target_include_directories(vc-receiver PRIVATE
  "${PROJECT_SOURCE_DIR}/proto" "${PROJECT_SOURCE_DIR}/vcam")
target_link_libraries(vc-receiver PRIVATE vc-common vc-pattern vc-npcap)
if(WIN32)
  target_link_libraries(vc-receiver PRIVATE ws2_32 mfplat mf mfuuid ole32 oleaut32)
endif()
vc_copy_to_build_root(vc-receiver)
