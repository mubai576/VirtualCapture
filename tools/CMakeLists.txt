# Keep existing binary directories while grouping development sources under tools/.
add_subdirectory(testwin "${CMAKE_BINARY_DIR}/testwin")
add_subdirectory(vcam_verify "${CMAKE_BINARY_DIR}/vcam_verify")
add_subdirectory(monitor "${CMAKE_BINARY_DIR}/monitor")
