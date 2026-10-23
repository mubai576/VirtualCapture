# vcam/CMakeLists.txt -- P4-3 DShow source filter (user-mode COM, no driver signing).
# Hand-rolled COM: no ATL, no strmbase (absent from modern Windows SDK). dshow.h + strmiids only.
add_library(vc-vcam SHARED vcam.cpp vcam.def)
target_include_directories(vc-vcam PRIVATE ${CMAKE_CURRENT_SOURCE_DIR} ${CMAKE_CURRENT_SOURCE_DIR}/../sender)
target_compile_features(vc-vcam PRIVATE cxx_std_17)
target_compile_definitions(vc-vcam PRIVATE UNICODE _UNICODE)
if(MSVC)
  target_compile_options(vc-vcam PRIVATE /utf-8)
endif()
if(WIN32)
  target_link_libraries(vc-vcam PRIVATE strmiids winmm)
endif()
set_target_properties(vc-vcam PROPERTIES
    RUNTIME_OUTPUT_DIRECTORY_RELEASE ${CMAKE_BINARY_DIR}/Release
    RUNTIME_OUTPUT_DIRECTORY_DEBUG   ${CMAKE_BINARY_DIR}/Debug)
# No post-build copy: use build/Release/vc-vcam.dll directly for registration.




