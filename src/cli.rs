use std::path::PathBuf;

use clap::{Parser, Subcommand};
use restreamer_core::{StartupError, config::Environment, maintenance, migration};

#[derive(Parser)]
#[command(
    name = "restreamer-core",
    about = "Castvane Core media orchestration and service administration",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CASTVANE_BUILD_COMMIT"), ")")
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the configured server (also the default command)
    Serve,
    /// Validate stored settings or a versioned JSON configuration with environment overrides
    ConfigCheck {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Write a private default JSON configuration for review
    ConfigInit { output: PathBuf },
    /// Inspect configured FFmpeg and FFprobe capabilities
    FfmpegCheck,
    /// Query the local HTTP liveness or readiness endpoint
    HealthCheck {
        #[arg(long, default_value = "http://127.0.0.1:8080/healthz")]
        url: url::Url,
    },
    /// Check integrity and migration history of the configured SQLite database
    DatabaseCheck,
    /// Take a consistent SQLite backup and print its SHA-256 checksum
    Backup { output: PathBuf },
    /// Verify and restore a backup to a new database path
    Restore {
        source: PathBuf,
        #[arg(long)]
        sha256: String,
    },
    /// Validate or import versioned datarhei JSON files into a new SQLite database
    Migrate {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        processes: PathBuf,
        #[arg(long)]
        zones: Option<PathBuf>,
        #[arg(long)]
        aliases: Option<PathBuf>,
        #[arg(long)]
        users: Option<PathBuf>,
        #[arg(long, requires = "users")]
        policies: Option<PathBuf>,
        #[arg(long, conflicts_with = "output", required_unless_present = "output")]
        dry_run: bool,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

pub async fn run(command: Option<Command>) -> Result<(), StartupError> {
    match command.unwrap_or(Command::Serve) {
        Command::Serve => super::serve().await?,
        Command::ConfigCheck { file } => {
            maintenance::validate_configuration(&Environment::read()?, file.as_deref()).await?;
            println!("Configuration valid");
        }
        Command::ConfigInit { output } => maintenance::init_configuration(output).await?,
        Command::FfmpegCheck => {
            let inventory = maintenance::ffmpeg_diagnostics(Environment::read()?).await?;
            println!("{}", serde_json::to_string_pretty(&inventory)?);
            if inventory["ffprobe_available"] != true {
                return Err("FFprobe was unavailable".into());
            }
        }
        Command::HealthCheck { url } => {
            if url.scheme() != "http"
                || !matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
                    && !matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
                || !matches!(url.path(), "/healthz" | "/readyz")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err("health check requires a loopback HTTP health or readiness URL".into());
            }
            let response = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(3))
                .build()?
                .get(url)
                .send()
                .await?;
            if !response.status().is_success() {
                return Err(
                    format!("health check returned HTTP {}", response.status().as_u16()).into(),
                );
            }
        }
        Command::DatabaseCheck => {
            maintenance::database_check(&Environment::read()?.db).await?;
            println!("Database integrity and migration history valid");
        }
        Command::Backup { output } => {
            println!(
                "{}",
                maintenance::backup(&Environment::read()?.db, &output).await?
            );
        }
        Command::Restore { source, sha256 } => {
            if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("expected a 64-digit hexadecimal SHA-256 checksum".into());
            }
            maintenance::restore(source, Environment::read()?.db, sha256).await?;
        }
        Command::Migrate {
            config,
            processes,
            zones,
            aliases,
            users,
            policies,
            dry_run: _,
            output,
        } => {
            let result = migration::migrate(
                migration::Source {
                    config: &config,
                    processes: &processes,
                    zones: zones.as_deref(),
                    aliases: aliases.as_deref(),
                    users: users.as_deref(),
                    policies: policies.as_deref(),
                },
                output.as_deref(),
                &Environment::read()?,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }
    Ok(())
}
