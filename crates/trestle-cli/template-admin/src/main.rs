mod admin;

fn main() -> std::process::ExitCode {
    trestle::project().install(admin::Directory).run()
}
