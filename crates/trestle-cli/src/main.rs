fn main() -> std::process::ExitCode {
    trestle_cli::main_with(std::env::args_os().skip(1).collect())
}
