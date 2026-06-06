fn main() {
    // Windows defaults the main-thread stack to 1 MiB, whereas
    // Linux/macOS default to 8 MiB. The large clap-derive command
    // tree builds a single big stack frame during argument parsing
    // and overflows that 1 MiB in unoptimized (debug) builds — even
    // `glass --version` crashes before doing any work. Reserve a
    // larger stack on the linked binaries so `glass` starts cleanly.
    //
    // Scoped to the MSVC toolchain (the only supported Windows
    // target — gpui requires it); `/STACK:<reserve-bytes>` is the
    // MSVC linker's stack-reserve switch.
    let is_msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if is_msvc {
        // 16 MiB reserve — comfortably above the debug-build frame.
        println!("cargo::rustc-link-arg-bins=/STACK:16777216");
    }
}
