//! The smallest Trestle project: `cargo run --example minimal -- check`.

fn main() -> std::process::ExitCode {
    trestle::project().run()
}
