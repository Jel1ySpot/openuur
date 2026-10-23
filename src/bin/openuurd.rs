use clap::{Parser, Subcommand};
use openuur::auth;
use openuur::config::Config;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "openuurd",
    version,
    about = "UU Remote native terminal and desktop host",
    long_about = "openuurd keeps this device online through UU signaling and serves its terminal and graphical desktop to official UU controllers and openuur clients over the native WebRTC protocol."
)]
struct Cli {
    #[arg(long, global = true, env = "OPENUUR_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, global = true, short = 'v')]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Put this device online and serve terminal and graphical desktop sessions.
    Run(RunArgs),
    /// Show the locally configured identity without printing the token.
    Status,
    /// Run 17 paste-safe host diagnostics (session, backend, permissions,
    /// encoders, account and service state).
    Doctor,
    /// Configure the portal capture source.
    Display {
        #[command(subcommand)]
        command: DisplayCmd,
    },
}

#[derive(Subcommand)]
enum DisplayCmd {
    /// Select monitor, window or virtual as the capture source.
    Source { source: String },
}

#[derive(clap::Args)]
struct RunArgs {
    #[arg(long)]
    device_name: Option<String>,
    #[arg(long)]
    device_id: Option<String>,
    /// Optional terminal unlock password requested by controllers.
    #[arg(long, env = "OPENUUR_TERMINAL_PASSWORD")]
    password: Option<String>,
    #[arg(long)]
    api_base: Option<String>,
    /// Portal capture source: monitor (default), window or virtual.
    #[arg(long, env = "UU_DISPLAY_SOURCE")]
    display_source: Option<String>,
    /// Capture backend: auto (default), portal, wlroots or x11. Persisted to
    /// the config file; `UU_DESKTOP_BACKEND` applies when the file has no
    /// value.
    #[arg(long)]
    desktop_backend: Option<String>,
    /// Directory the viewer's file window opens on and receives into.
    #[arg(long, env = "UU_FILE_SAVE_PATH")]
    file_save_path: Option<PathBuf>,
    /// Do not connect to UU signaling; the device stays offline.
    #[arg(long)]
    no_online: bool,
}

#[tokio::main]
async fn main() {
    if let Err(err) = real_main().await {
        eprintln!("openuurd: {err:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    if matches!(cli.command, Command::Doctor) {
        // The report must be the only diagnostic output.
        std::env::set_var("RUST_LOG", "openuur=warn,openuurd=warn");
    }
    init_logging(cli.verbose);
    if let Some(path) = &cli.config {
        std::env::set_var("OPENUUR_CONFIG", path);
    }
    if matches!(cli.command, Command::Doctor) {
        // Diagnosis must not generate ids or rewrite the config file.
        openuur::doctor::run().await.map_err(anyhow::Error::msg)?;
        return Ok(());
    }
    let mut config = load_config(cli.config.as_deref())?;
    config.ensure_client_id();
    config.ensure_system_id();

    match cli.command {
        Command::Run(args) => {
            let mut changed = false;
            if let Some(api_base) = args.api_base {
                config.api_base = api_base;
            }
            if let Some(name) = args.device_name {
                config.device_name = Some(name);
            }
            if let Some(id) = args.device_id {
                config.device_id = Some(id);
            }
            if let Some(source) = args.display_source {
                // Validate before going online.
                openuur::desktop::SourceKind::parse(&source).map_err(anyhow::Error::msg)?;
                config.display_source = Some(source);
                changed = true;
            }
            if let Some(backend) = args.desktop_backend {
                let choice =
                    openuur::desktop::BackendChoice::parse(&backend).map_err(anyhow::Error::msg)?;
                config.desktop_backend = Some(choice.as_str().to_owned());
                changed = true;
            }
            if let Some(path) = args.file_save_path {
                config.file_save_path = Some(path.to_string_lossy().into_owned());
                changed = true;
            }
            if changed {
                config.save()?;
            }
            config.require_token()?;
            if args.no_online {
                let device_id = config.ensure_device_id()?;
                let device_name = config.ensure_device_name();
                println!("openuurd device: {device_name} ({device_id})");
                println!("online: disabled");
                return Ok(());
            }
            auth::ensure_registered(&mut config).await?;
            let device_id = config.ensure_device_id()?;
            let device_name = config.ensure_device_name();
            println!("openuurd device: {device_name} ({device_id})");
            match openuur::desktop::detect_backend_selection(config.desktop_backend()).await {
                Ok((selection, session)) => {
                    println!("desktop session: {}", session.describe());
                    println!(
                        "desktop capture: {}",
                        capture_line(selection, config.display_source())
                    );
                }
                Err(error) => println!("desktop capture: unavailable ({error})"),
            }
            openuur::uu_native::host::run(config, args.password).await?;
        }
        Command::Doctor => unreachable!("doctor is handled before loading the config"),
        Command::Status => {
            let device_id = config.ensure_device_id()?;
            let device_name = config.ensure_device_name();
            println!("api_base: {}", config.api_base);
            println!("user_id: {}", config.user_id.as_deref().unwrap_or("-"));
            println!("device: {device_name} ({device_id})");
            println!("desktop backend: {}", config.desktop_backend().as_str());
            match openuur::desktop::detect_backend_selection(config.desktop_backend()).await {
                Ok((selection, session)) => {
                    println!("desktop session: {}", session.describe());
                    println!(
                        "desktop capture: {}",
                        capture_line(selection, config.display_source())
                    );
                }
                Err(error) => {
                    println!("desktop session: {error}");
                    println!("desktop capture: unavailable");
                }
            }
            println!("display source: {}", config.display_source().as_str());
            println!("file save path: {}", config.file_save_path().display());
            println!(
                "token: {}",
                if config
                    .token
                    .as_deref()
                    .map(|t| !t.is_empty())
                    .unwrap_or(false)
                {
                    "saved"
                } else {
                    "missing"
                }
            );
        }
        Command::Display { command } => match command {
            DisplayCmd::Source { source } => {
                let kind =
                    openuur::desktop::SourceKind::parse(&source).map_err(anyhow::Error::msg)?;
                config.display_source = Some(kind.as_str().to_owned());
                config.save()?;
                println!("display source: {}", kind.as_str());
            }
        },
    }
    Ok(())
}

/// One line that never presents an unimplemented backend as usable.
fn capture_line(
    selection: openuur::desktop::BackendSelection,
    source: openuur::desktop::SourceKind,
) -> String {
    let backend = selection.effective.label();
    if !selection.effective.implemented() {
        return format!("{backend} (backend not implemented yet; desktop offers will be rejected)");
    }
    let mut line = if selection.fallback {
        format!(
            "{backend} ({} requested; native backend not implemented)",
            selection.requested.label()
        )
    } else {
        backend.to_owned()
    };
    if selection.effective == openuur::desktop::Backend::X11
        && source != openuur::desktop::SourceKind::Monitor
    {
        line.push_str(" (X11 captures a whole monitor; window/virtual need portal)");
    }
    line
}

fn init_logging(verbose: bool) {
    let filter = if verbose {
        "openuur=debug,openuurd=debug"
    } else {
        "openuur=info,openuurd=info"
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .try_init();
}

fn load_config(path: Option<&std::path::Path>) -> anyhow::Result<Config> {
    if let Some(path) = path {
        if path.exists() {
            return Ok(Config::load_from(path)?);
        }
    }
    Ok(Config::load()?)
}
