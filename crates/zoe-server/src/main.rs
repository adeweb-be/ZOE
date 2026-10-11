//! The `zoe` binary. It runs [`zoe_server::cli::main`], the code that the
//! console script of the `zoedb` wheel also runs.

use std::process::ExitCode;

fn main() -> ExitCode {
    let code = zoe_server::cli::main(std::env::args_os().collect());
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}
