use std::process::ExitCode;

fn main() -> ExitCode {
    citadel::init_tracing();
    match citadel::cli::run_from_args() {
        Ok(()) => ExitCode::SUCCESS,
        // Already explained on the command's output.
        Err(err) if err.is::<citadel::cli::SilentExit>() => ExitCode::FAILURE,
        // The same report `fn main() -> anyhow::Result<()>` prints.
        Err(err) => {
            eprintln!("Error: {err:?}");
            ExitCode::FAILURE
        }
    }
}
