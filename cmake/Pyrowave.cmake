# Pinned PyroWave (MIT) + Granite runtime subset; Vulkan compute codec, no system install.
# Sources are fetched at configure time (hash-checked) and built by ExternalProject so the
# Granite CMake graph stays out of this project.
include_guard(GLOBAL)
include(ExternalProject)

function(vc_pyrowave_fetch url archive sha256 dest)
  if(NOT EXISTS "${archive}")
    file(DOWNLOAD "${url}" "${archive}" TIMEOUT 300 TLS_VERIFY ON
      EXPECTED_HASH "SHA256=${sha256}")
  endif()
  file(SHA256 "${archive}" actual)
  if(NOT actual STREQUAL sha256)
    message(FATAL_ERROR "PyroWave dependency hash mismatch: ${archive}")
  endif()
  if(EXISTS "${dest}")
    # Granite's archive carries empty placeholder directories for its submodules.
    file(GLOB existing "${dest}/*")
    list(LENGTH existing count)
    if(count EQUAL 0)
      file(REMOVE_RECURSE "${dest}")
    endif()
  endif()
  if(NOT EXISTS "${dest}")
    set(staging "${archive}.d")
    file(REMOVE_RECURSE "${staging}")
    file(MAKE_DIRECTORY "${staging}")
    file(ARCHIVE_EXTRACT INPUT "${archive}" DESTINATION "${staging}")
    file(GLOB entries "${staging}/*")
    list(LENGTH entries count)
    if(NOT count EQUAL 1)
      message(FATAL_ERROR "PyroWave dependency archive layout: ${archive}")
    endif()
    get_filename_component(top "${entries}" NAME)
    get_filename_component(parent "${dest}" DIRECTORY)
    file(MAKE_DIRECTORY "${parent}")
    file(RENAME "${staging}/${top}" "${dest}")
    file(REMOVE_RECURSE "${staging}")
  endif()
endfunction()

function(vc_enable_pyrowave target)
  if(NOT TARGET vc-pyrowave)
    set(deps "${CMAKE_BINARY_DIR}/_deps")
    file(MAKE_DIRECTORY "${deps}/downloads")
    set(pw_commit a32b05e03d70826b19d4d11ff74fe166ed74371b)
    set(gr_commit 53090429539bf3404250d18fe58db971f0907c04)
    set(volk_commit 47cddf7ed97b94118a08aacb548a411188e016cc)
    set(vkh_commit 6802bb4733b63ed5efd3adb308a6c885ef180ea1)
    set(source "${deps}/pyrowave-src")
    vc_pyrowave_fetch("https://codeload.github.com/Themaister/pyrowave/tar.gz/${pw_commit}"
      "${deps}/downloads/pyrowave-${pw_commit}.tar.gz"
      "38245f2e76c6656907d39cd50b13dcb3d73ea8cc174eb28b6b38d7106db3e2f8" "${source}")
    vc_pyrowave_fetch("https://codeload.github.com/Themaister/Granite/tar.gz/${gr_commit}"
      "${deps}/downloads/granite-${gr_commit}.tar.gz"
      "51765130fa3a476e1a7c82d1656d9d5f58a53a2e7642483df9b09d0d7dd8d7a8" "${source}/Granite")
    vc_pyrowave_fetch("https://codeload.github.com/zeux/volk/tar.gz/${volk_commit}"
      "${deps}/downloads/volk-${volk_commit}.tar.gz"
      "ed771f9132ea077af0abc74d29f415a81f181f83c3ac5e0f67b2f2149976fe2d" "${source}/Granite/third_party/volk")
    vc_pyrowave_fetch("https://codeload.github.com/KhronosGroup/Vulkan-Headers/tar.gz/${vkh_commit}"
      "${deps}/downloads/vulkan-headers-${vkh_commit}.tar.gz"
      "9044fbb356160e43201dc2094acb44c8a906722a63a35f48fc0f051579db9bb6" "${source}/Granite/third_party/khronos/vulkan-headers")

    set(binary "${deps}/pyrowave-build")
    set(gen_args CMAKE_GENERATOR "${CMAKE_GENERATOR}")
    if(CMAKE_GENERATOR_PLATFORM)
      list(APPEND gen_args CMAKE_GENERATOR_PLATFORM "${CMAKE_GENERATOR_PLATFORM}")
    endif()
    if(CMAKE_GENERATOR_TOOLSET)
      list(APPEND gen_args CMAKE_GENERATOR_TOOLSET "${CMAKE_GENERATOR_TOOLSET}")
    endif()
    ExternalProject_Add(vc-pyrowave-build SOURCE_DIR "${source}" BINARY_DIR "${binary}"
      DOWNLOAD_COMMAND "" UPDATE_COMMAND "" PREFIX "${deps}/pyrowave-prefix"
      ${gen_args}
      CMAKE_ARGS "-DCMAKE_BUILD_TYPE=${CMAKE_BUILD_TYPE}"
        -DPYROWAVE_DEVEL=OFF -DPYROWAVE_UTILS=OFF
      BUILD_COMMAND "${CMAKE_COMMAND}" --build <BINARY_DIR> --config $<CONFIG> --target pyrowave-shared
      INSTALL_COMMAND "")

    add_library(vc-pyrowave SHARED IMPORTED GLOBAL)
    foreach(config Debug Release RelWithDebInfo MinSizeRel)
      string(TOUPPER "${config}" upper)
      set_property(TARGET vc-pyrowave PROPERTY IMPORTED_LOCATION_${upper}
        "${binary}/${config}/libpyrowave-shared-1.dll")
      set_property(TARGET vc-pyrowave PROPERTY IMPORTED_IMPLIB_${upper}
        "${binary}/${config}/pyrowave-shared.lib")
    endforeach()
    set_property(TARGET vc-pyrowave PROPERTY IMPORTED_LOCATION
      "${binary}/Release/libpyrowave-shared-1.dll")
    set_property(TARGET vc-pyrowave PROPERTY IMPORTED_IMPLIB
      "${binary}/Release/pyrowave-shared.lib")
    set_property(TARGET vc-pyrowave PROPERTY INTERFACE_INCLUDE_DIRECTORIES
      "${source};${source}/Granite/third_party/khronos/vulkan-headers/include")
    install(FILES "$<TARGET_FILE:vc-pyrowave>" DESTINATION . COMPONENT Runtime)
    install(FILES "${source}/LICENSE" DESTINATION licenses RENAME pyrowave-LICENSE.txt COMPONENT Runtime)
    install(FILES "${source}/Granite/LICENSE" DESTINATION licenses RENAME granite-LICENSE.txt COMPONENT Runtime)
  endif()
  target_link_libraries(${target} PRIVATE vc-pyrowave)
  target_compile_definitions(${target} PRIVATE VC_HAS_PYROWAVE=1)
  add_dependencies(${target} vc-pyrowave-build)
  add_custom_command(TARGET ${target} POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy_if_different
      "$<TARGET_FILE:vc-pyrowave>" "$<TARGET_FILE_DIR:${target}>"
    COMMAND ${CMAKE_COMMAND} -E copy_if_different
      "$<TARGET_FILE:vc-pyrowave>" "${CMAKE_BINARY_DIR}"
    VERBATIM)
endfunction()
