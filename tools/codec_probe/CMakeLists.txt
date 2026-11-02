# Development-only probe; never installed/deployed or selected as a codec fallback.
add_executable(vc-codec-probe codec_probe.cpp)
vc_configure_target(vc-codec-probe)
target_link_libraries(vc-codec-probe PRIVATE mfplat mf mfuuid ole32 oleaut32)

add_executable(vc-codec-roundtrip roundtrip.cpp)
vc_configure_target(vc-codec-roundtrip)
target_link_libraries(vc-codec-roundtrip PRIVATE mfplat mf mfuuid ole32 oleaut32)
