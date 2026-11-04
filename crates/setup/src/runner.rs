use std::io;
use std::process::{Command, Output};

/// Runs external programs; faked in tests so nothing real is started.
pub trait Runner {
    /// Runs `program` with `args` and waits for it.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the program cannot be started.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output>;
}

/// Runs programs for real.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        Command::new(program).args(args).output()
    }
}
