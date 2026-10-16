//! `pilot-migrate`: the migration-only binary (design doc §14.2, §16.1).
//! Replays history without compiling the web service.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use sqlx::Connection;
use sqlx::postgres::PgConnectOptions;

#[derive(Parser)]
#[command(about = "Plan or apply the pilot's committed migrations")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Show what would run without changing anything.
    #[arg(long)]
    plan: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let options: PgConnectOptions = args
        .database_url
        .parse()
        .map_err(|e| format!("invalid DATABASE_URL: {e}"))?;
    let history = pilot_migrations::history().map_err(|e| e.to_string())?;
    if args.plan {
        let mut conn = sqlx::PgConnection::connect_with(&options)
            .await
            .map_err(|e| e.to_string())?;
        let plan = trestle_historical::plan(&mut conn, &history)
            .await
            .map_err(|e| e.to_string())?;
        for entry in &plan.entries {
            println!("{:<28} {:?}", entry.migration.to_string(), entry.state);
        }
        return Ok(());
    }
    let applied = trestle_historical::migrate(&options, &history)
        .await
        .map_err(|e| e.to_string())?;
    if applied.is_empty() {
        println!("No migrations to apply.");
    }
    for migration in applied {
        println!("Applied {migration}");
    }
    Ok(())
}
