use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use loremaster::config::Paths;
use loremaster::enable::{self, Agent};
use loremaster::{hook, login, server};

/// Cross-device, cross-agent memory for AI agents.
#[derive(Parser)]
#[command(name = "loremaster", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Handle one agent hook event (run by the agent)
    Hook { agent: String, event: String },
    /// Run the long-running service
    Daemon,
    /// Install hooks for an agent
    Enable {
        #[arg(value_enum)]
        agent: Agent,
    },
    /// Sign this machine in with GitHub and register it as a device
    Login,
    /// Development tools
    #[cfg(feature = "dev")]
    Dev {
        #[command(subcommand)]
        command: loremaster::dev::DevCommand,
    },
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("{message}");
    ExitCode::FAILURE
}

fn run_hook(agent: &str, event: &str) -> ExitCode {
    let mut input = String::new();
    if let (Ok(paths), Ok(_)) = (
        Paths::from_env(),
        std::io::stdin().read_to_string(&mut input),
    ) && let Some(output) = hook::run(&paths, agent, event, &input)
    {
        // The agent may have closed the pipe; a hook still must not fail.
        let _ = writeln!(std::io::stdout(), "{output}");
    }
    ExitCode::SUCCESS
}

fn run_enable(agent: Agent) -> Result<String, String> {
    let paths = Paths::from_env().map_err(|e| e.to_string())?;
    let user_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    enable::enable(agent, &paths, &user_home).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Hook { agent, event } => run_hook(&agent, &event),
        Command::Daemon => match Paths::from_env()
            .map_err(|e| e.to_string())
            .and_then(|paths| server::run(&paths).map_err(|e| e.to_string()))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        },
        Command::Enable { agent } => match run_enable(agent) {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        Command::Login => {
            match Paths::from_env()
                .map_err(login::LoginError::from)
                .and_then(|paths| {
                    login::run(&paths, &mut std::io::stdin().lock(), &mut std::io::stdout())
                }) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(format!("error: {e}")),
            }
        }
        #[cfg(feature = "dev")]
        Command::Dev { command } => loremaster::dev::run(command),
    }
}
