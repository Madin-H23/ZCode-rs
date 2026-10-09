//! The standalone CLI binary. Everything it does is the library's CLI entry
//! ([`zcode_cli_rust::cli`]), which the merged `zcode-host` binary also runs.
fn main() -> std::process::ExitCode {
    let mut argv = std::env::args_os();
    let program = argv.next().unwrap_or_default();
    zcode_cli_rust::cli::main(program, argv.collect())
}
