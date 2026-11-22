use anyhow::Result;
use ch_core::Config;
use clap::Parser;
use codehaven::cli::{Cli, Command, HookCommand};
use codehaven::{admin, doctor, serve, wiring};
use std::process::ExitCode;
use std::sync::Arc;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    // The hook runs on every push and must stay quiet and fast: no config, no logging setup.
    if let Command::Hook { command } = &cli.command {
        let name = match command {
            HookCommand::PreReceive => "pre-receive",
            HookCommand::PostReceive => "post-receive",
        };
        // The client lives in ch-git beside the server half it talks to.
        let code = ch_git::hooks::client::run(name);
        return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
    }

    let config = Config::load(Some(&cli.config))?;
    match cli.command {
        Command::Serve { migrate } => {
            serve::init_tracing(&config);
            serve::serve(config, migrate).await?;
        }
        Command::Migrate => {
            serve::init_tracing(&config);
            serve::migrate(&config).await?;
            println!("migrations are up to date");
        }
        Command::Admin { command } => {
            let pool = serve::connect(&config).await?;
            let state = wiring::build_state(Arc::new(config), pool).await?;
            admin::run(&state, command).await?;
        }
        Command::Healthcheck => {
            return Ok(if doctor::healthcheck(&config).await { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        Command::Doctor => {
            return Ok(if doctor::run(&config).await { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        Command::Hook { .. } => unreachable!("handled above"),
    }
    Ok(ExitCode::SUCCESS)
}
