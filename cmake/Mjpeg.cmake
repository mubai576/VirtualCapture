# Static, pinned TurboJPEG with SIMD; no runtime DLL or system-wide installation.
include_guard(GLOBAL)
include(ExternalProject)

function(vc_download_checked url path sha256)
  if(NOT EXISTS "${path}")
    file(DOWNLOAD "${url}" "${path}" TIMEOUT 90 TLS_VERIFY ON
      EXPECTED_HASH "SHA256=${sha256}")
  endif()
  file(SHA256 "${path}" actual)
  if(NOT actual STREQUAL sha256)
    message(FATAL_ERROR "MJPEG dependency hash mismatch: ${path}")
  endif()
endfunction()

function(vc_enable_mjpeg target)
  if(NOT TARGET vc-turbojpeg)
    set(deps "${CMAKE_BINARY_DIR}/_deps")
    file(MAKE_DIRECTORY "${deps}/downloads")
    set(archive "${deps}/downloads/libjpeg-turbo-3.1.2.tar.gz")
    vc_download_checked("https://codeload.github.com/libjpeg-turbo/libjpeg-turbo/tar.gz/refs/tags/3.1.2"
      "${archive}" "560f6338b547544c4f9721b18d8b87685d433ec78b3c644c70d77adad22c55e6")
    set(source "${deps}/libjpeg-turbo-3.1.2")
    if(NOT EXISTS "${source}/CMakeLists.txt")
      file(ARCHIVE_EXTRACT INPUT "${archive}" DESTINATION "${deps}")
    endif()
    set(nasm_archive "${deps}/downloads/nasm-2.16.03-win64.zip")
    vc_download_checked("https://www.nasm.us/pub/nasm/releasebuilds/2.16.03/win64/nasm-2.16.03-win64.zip"
      "${nasm_archive}" "3ee4782247bcb874378d02f7eab4e294a84d3d15f3f6ee2de2f47a46aa7226e6")
    if(NOT EXISTS "${deps}/nasm-2.16.03/nasm.exe")
      file(ARCHIVE_EXTRACT INPUT "${nasm_archive}" DESTINATION "${deps}")
    endif()
    set(binary "${deps}/libjpeg-turbo-build")
    ExternalProject_Add(vc-turbojpeg-build SOURCE_DIR "${source}" BINARY_DIR "${binary}"
      DOWNLOAD_COMMAND "" UPDATE_COMMAND ""
      CMAKE_ARGS -DENABLE_SHARED=OFF -DENABLE_STATIC=ON -DWITH_TURBOJPEG=ON
        -DWITH_TOOLS=OFF -DWITH_TESTS=OFF -DWITH_SIMD=ON -DREQUIRE_SIMD=ON -DWITH_CRT_DLL=ON
        "-DCMAKE_ASM_NASM_COMPILER=${deps}/nasm-2.16.03/nasm.exe"
        "-DCMAKE_BUILD_TYPE=${CMAKE_BUILD_TYPE}" "-DCMAKE_C_FLAGS=/utf-8"
      BUILD_COMMAND "${CMAKE_COMMAND}" --build <BINARY_DIR> --config $<CONFIG> --target turbojpeg-static
      INSTALL_COMMAND "")
    add_library(vc-turbojpeg STATIC IMPORTED GLOBAL)
    foreach(config Debug Release RelWithDebInfo MinSizeRel)
      string(TOUPPER "${config}" upper)
      set_property(TARGET vc-turbojpeg PROPERTY IMPORTED_LOCATION_${upper} "${binary}/${config}/turbojpeg-static.lib")
    endforeach()
    set_property(TARGET vc-turbojpeg PROPERTY IMPORTED_LOCATION "${binary}/turbojpeg-static.lib")
    set_property(TARGET vc-turbojpeg PROPERTY INTERFACE_INCLUDE_DIRECTORIES "${source}/src")
    add_dependencies(vc-turbojpeg vc-turbojpeg-build)
    install(FILES "${source}/LICENSE.md" DESTINATION licenses RENAME libjpeg-turbo-LICENSE.md COMPONENT Runtime)
    install(FILES "${source}/README.ijg" DESTINATION licenses RENAME libjpeg-turbo-README.ijg COMPONENT Runtime)
  endif()
  target_link_libraries(${target} PRIVATE vc-turbojpeg)
  target_compile_definitions(${target} PRIVATE VC_HAS_MJPEG=1)
endfunction()
