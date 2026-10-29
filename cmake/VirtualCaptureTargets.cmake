include_guard(GLOBAL)

function(vc_configure_target target)
  target_compile_features(${target} PRIVATE cxx_std_17)
  if(MSVC)
    target_compile_options(${target} PRIVATE /utf-8)
  endif()
  # VS adds the configuration directory; single-config generators use this root.
  # Follow -B instead of hard-coding a build directory in the source tree.
  set_target_properties(${target} PROPERTIES
    RUNTIME_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}")
endfunction()

function(vc_copy_to_build_root target)
  # Keep the frozen build/vc-{sender,receiver}.exe entry points for all generators.
  add_custom_command(TARGET ${target} POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy_if_different
      "$<TARGET_FILE:${target}>" "${CMAKE_BINARY_DIR}/$<TARGET_FILE_NAME:${target}>"
    VERBATIM)
endfunction()
