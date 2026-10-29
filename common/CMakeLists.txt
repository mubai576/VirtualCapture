# One copy of the bit-exact CRC32/FNV algorithms used by both transport ends.
add_library(vc-common INTERFACE)
target_include_directories(vc-common INTERFACE "${CMAKE_CURRENT_SOURCE_DIR}")
