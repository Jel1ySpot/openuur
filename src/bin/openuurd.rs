use clap::{Parser, Subcommand};
use openuur::auth;
use openuur::config::Config;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "openuurd",
    version,
    about = "UU Remote native terminal and Wayland desktop host",
    long_about = "openuurd keeps this device online through UU signaling and serves its terminal and Wayland desktop to official UU controllers and openuur clients over the native WebRTC protocol."
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
    /// Put this device online and serve terminal and Wayland desktop sessions.
    Run(RunArgs),
    /// Show the locally configured identity without printing the token.
    Status,
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
    init_logging(cli.verbose);
    if let Some(path) = &cli.config {
        std::env::set_var("OPENUUR_CONFIG", path);
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
            match openuur::desktop::detect_backend() {
                Ok(backend) => println!("desktop capture: {backend:?}"),
                Err(error) => println!("desktop capture: unavailable ({error})"),
            }
            openuur::uu_native::host::run(config, args.password).await?;
        }
        Command::Status => {
            let device_id = config.ensure_device_id()?;
            let device_name = config.ensure_device_name();
            println!("api_base: {}", config.api_base);
            println!("user_id: {}", config.user_id.as_deref().unwrap_or("-"));
            println!("device: {device_name} ({device_id})");
            println!(
                "desktop: {}",
                match openuur::desktop::detect_backend() {
                    Ok(backend) => format!("{backend:?}"),
                    Err(error) => format!("unavailable ({error})"),
                }
            );
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
            let text = std::fs::read_to_string(path)?;
            return Ok(toml::from_str(&text)?);
        }
    }
    Ok(Config::load()?)
}
