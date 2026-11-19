// vc_enable_pyrowave() requires a link target.  The Rust package never links
// this static library; keeping a real object avoids MSVC's empty-library
// warning while the target stays EXCLUDE_FROM_ALL.
extern "C" int vc_package_native_probe(void) { return 0; }
