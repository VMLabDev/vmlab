// Buildout in progress: items land before their consumers. Remove once the
// CLI surface is complete (PRD §12).
#![allow(dead_code)]

mod cli;
mod config;
mod paths;

fn main() -> std::process::ExitCode {
    cli::run()
}
