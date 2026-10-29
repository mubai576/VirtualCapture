# One runtime directory for the portable package; no import libraries or PDBs.
install(TARGETS vc-sender vc-receiver vc-vcam vc-testwin vc-monitor
  vc-monitor-diag vc-vcam-verify
  RUNTIME DESTINATION . COMPONENT Runtime)

# Install the toolchain's redistributable MSVC DLLs next to our executables.
set(CMAKE_INSTALL_SYSTEM_RUNTIME_DESTINATION .)
set(CMAKE_INSTALL_SYSTEM_RUNTIME_COMPONENT Runtime)
include(InstallRequiredSystemLibraries)
