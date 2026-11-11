fn main() -> std::process::ExitCode {
    // The cargo shim and the rustc wrapper are links to this binary.
    if ferry::shim::invoked_as_cargo() {
        return ferry::shim::main();
    }
    if ferry::remap::invoked_as_wrapper() {
        return ferry::remap::main();
    }
    ferry::cli::main()
}
