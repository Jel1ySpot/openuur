use clap::{Args, Parser, Subcommand};
use crossterm::terminal as ct;
use openuur::api::ApiClient;
use openuur::auth;
use openuur::config::Config;
use openuur::error::{Error, Result};
use openuur::terminal::session::{OpenRequest, SessionEvent, SessionManager};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    name = "openuur",
    version,
    about = "UU Remote native terminal client",
    long_about = "openuur logs into a UU Remote account, manages devices and wallpapers, and connects to remote terminals through UU signaling and WebRTC."
)]
struct Cli {
    #[arg(long, global = true, env = "OPENUUR_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    api_base: Option<String>,
    #[arg(long, global = true, short = 'v')]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log in with a mobile verification code or a UU access token.
    Login(LoginArgs),
    /// Forget the saved account token.
    Logout,
    /// Show the current account identity.
    Whoami,
    /// List devices in the account.
    Devices {
        #[arg(long)]
        json: bool,
    },
    /// Device management.
    Device {
        #[command(subcommand)]
        command: DeviceCmd,
    },
    /// Connect to a device's UU native terminal.
    Term(TermArgs),
    /// Install and enable openuurd as a systemd service.
    ///
    /// Run as a normal user for a user service; run through sudo for a system
    /// service for the invoking account.
    Install(InstallArgs),
}

#[derive(Args)]
struct InstallArgs {
    /// Path to the openuurd binary (default: next to openuur or on PATH).
    #[arg(long)]
    binary: Option<PathBuf>,
    /// System install only: account that owns the graphical session.
    #[arg(long)]
    user: Option<String>,
    /// Write and enable the unit without starting it now.
    #[arg(long)]
    no_start: bool,
}

#[derive(Args)]
struct LoginArgs {
    #[arg(long)]
    mobile: Option<String>,
    #[arg(long)]
    code: Option<String>,
    /// Only request an SMS verification code; do not log in yet.
    #[arg(long)]
    send_code: bool,
    #[arg(long, default_value = "86")]
    country_code: String,
    /// UU access token (by_uu_token).
    #[arg(long)]
    token: Option<String>,
    /// MuMu access token (by_mumu_token).
    #[arg(long)]
    mumu_token: Option<String>,
    /// QR login jump URL (advanced; requires --qr-token).
    #[arg(long)]
    qr_jump_url: Option<String>,
    #[arg(long)]
    qr_token: Option<String>,
    /// Interactive QR login: print a terminal QR code and wait for the UU
    /// mobile app to confirm.
    #[arg(long)]
    qr: bool,
}

#[derive(Subcommand)]
enum DeviceCmd {
    /// List devices.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Rename a device.
    Rename { device: String, name: String },
    /// Set a device wallpaper by URL, or upload a file first.
    Wallpaper {
        device: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        upload_url: Option<String>,
        /// Upload and print the resulting URL without updating the device.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show raw device detail JSON.
    Info { device: String },
}

#[derive(Args)]
struct TermArgs {
    /// Target device name or id. Omit with --local.
    device: Option<String>,
    /// Run a local shell instead of connecting to a device.
    #[arg(long)]
    local: bool,
    #[arg(long)]
    shell: Option<String>,
    #[arg(long)]
    session_id: Option<u16>,
}

#[tokio::main]
async fn main() {
    if let Err(err) = real_main().await {
        eprintln!("openuur: {err}");
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
    let command = match cli.command {
        Command::Install(args) => {
            let report = openuur::install::run(openuur::install::InstallOptions {
                binary: args.binary,
                config: cli.config.clone(),
                user: args.user,
                no_start: args.no_start,
            })
            .map_err(anyhow::Error::msg)?;
            print_install_report(&report);
            return Ok(());
        }
        other => other,
    };
    let mut config = load_config(cli.config.as_deref())?;
    config.ensure_client_id();
    config.ensure_system_id();
    config.ensure_device_id()?;
    if let Some(api_base) = cli.api_base.clone() {
        config.api_base = api_base;
    }
    match command {
        Command::Login(args) => do_login(&mut config, args).await?,
        Command::Logout => {
            if let Ok(api) = api_client(&config, true) {
                let _ = api.logout().await;
            }
            config.token = None;
            config.user_id = None;
            config.save()?;
            println!("logged out");
        }
        Command::Whoami => {
            let info = auth::refresh_identity(&mut config).await?;
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        Command::Devices { json } => {
            let devices = fetch_devices(&mut config).await?;
            print_devices(&devices, json);
        }
        Command::Device { command } => match command {
            DeviceCmd::List { json } => {
                let devices = fetch_devices(&mut config).await?;
                print_devices(&devices, json);
            }
            DeviceCmd::Rename { device, name } => {
                rename_device(&mut config, &device, &name).await?;
            }
            DeviceCmd::Wallpaper {
                device,
                url,
                file,
                upload_url,
                dry_run,
            } => {
                set_wallpaper(&mut config, &device, url, file, upload_url, dry_run).await?;
            }
            DeviceCmd::Info { device } => {
                auth::ensure_registered(&mut config).await?;
                let api = api_client(&config, true)?;
                let resolved = api.resolve_device(&device).await?;
                let detail = api.device_detail(&resolved.device_id).await?;
                println!("{}", serde_json::to_string_pretty(&detail)?);
            }
        },
        Command::Term(args) => {
            if args.local {
                run_local_terminal(&config, &args).await?;
            } else {
                let query = args
                    .device
                    .as_deref()
                    .ok_or_else(|| Error::Protocol("term needs a device name or id".to_owned()))?;
                auth::ensure_registered(&mut config).await?;
                let api = api_client(&config, true)?;
                let target = api.resolve_device(query).await?;
                openuur::uu_native::viewer::run(
                    &config,
                    &target.device_id,
                    args.shell.clone(),
                    args.session_id.map(u32::from),
                )
                .await?;
            }
        }
        Command::Install(_) => unreachable!("install is handled before loading the config"),
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

fn api_client(config: &Config, require_token: bool) -> Result<ApiClient> {
    let token = if require_token {
        Some(config.require_token()?)
    } else {
        config.token.clone()
    };
    ApiClient::from_config(config, token)
}

async fn do_login(config: &mut Config, args: LoginArgs) -> anyhow::Result<()> {
    if args.send_code || (args.mobile.is_some() && args.code.is_none()) {
        let mobile = args
            .mobile
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--send-code needs --mobile"))?;
        auth::ensure_registered(config).await?;
        let api = api_client(config, false)?;
        let response = api.request_mobile_code(&args.country_code, mobile).await?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        if args.code.is_none() {
            println!("verification code requested; run `openuur login --mobile {mobile} --code <code>` next");
            return Ok(());
        }
    }
    if args.qr {
        run_qr_login(config).await?;
    } else if let Some(token) = args.token {
        auth::login_token(config, &token).await?;
    } else if let Some(token) = args.mumu_token {
        auth::login_mumu_token(config, &token).await?;
    } else if let (Some(mobile), Some(code)) = (args.mobile, args.code) {
        auth::login_mobile(config, &args.country_code, &mobile, &code).await?;
    } else if let (Some(jump), Some(token)) = (args.qr_jump_url, args.qr_token) {
        let api = ApiClient::new(&config.api_base, None)?;
        let outcome = api.login_qrcode(&jump, &token).await?;
        auth::apply_login(config, &outcome)?;
    } else {
        anyhow::bail!(
            "login needs --qr, --mobile with --code, or --token, or --mumu-token, or --qr-jump-url with --qr-token"
        );
    }

    if let Ok(info) = auth::refresh_identity(config).await {
        let user = openuur::api::value_user_id(&info).unwrap_or_else(|| "-".to_owned());
        let nickname = openuur::util::find_string(
            &info,
            &["nickname", "nickName", "username", "userName", "name"],
        )
        .unwrap_or_else(|| "-".to_owned());
        println!("logged in: user={user} nickname={nickname}");
    } else {
        println!("logged in; token saved");
    }
    Ok(())
}

/// Interactive QR login: print a terminal QR, long-poll the scan status and
/// save the resulting token. The QR content is a login credential and is only
/// shown in this terminal.
async fn run_qr_login(config: &mut Config) -> anyhow::Result<()> {
    use std::time::Duration;
    // The QR endpoints are anonymous, but the device identity must exist like
    // the official client's login window.
    auth::ensure_registered(config).await?;
    let api = ApiClient::from_config(config, None)?;
    loop {
        let response = api.qrcode_gen_login().await?;
        let data = response.get("data").unwrap_or(&response);
        let url = data
            .get("qrcode_jump_url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("qrcode gen returned no qrcode_jump_url"))?
            .to_owned();
        let ticket = data
            .get("token")
            .or_else(|| data.get("status_query_ticket"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("qrcode gen returned no token"))?
            .to_owned();
        println!(
            "\nScan with the UU Remote mobile app (网易UU远程 App 扫码登录):\n{}",
            render_qr(&url)?
        );
        let mut status = 1i64;
        let mut expired = false;
        for _ in 0..150 {
            let response = match api.qrcode_login_status(&ticket, &url, status).await {
                Ok(response) => response,
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if response.get("code").and_then(serde_json::Value::as_i64) == Some(1190) {
                expired = true;
                break;
            }
            let data = response.get("data").unwrap_or(&response);
            let next = data
                .get("status")
                .or_else(|| response.get("status"))
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(status);
            if next == status {
                continue;
            }
            status = next;
            match status {
                2 => println!("已扫码，请在手机上确认 (scanned, confirm on the phone)"),
                3 | 4 => {
                    let outcome = api.login_qrcode(&url, &ticket).await?;
                    auth::apply_login(config, &outcome)?;
                    return Ok(());
                }
                _ => {
                    expired = true;
                    break;
                }
            }
        }
        println!(
            "{}",
            if expired {
                "QR code expired; generating a new one"
            } else {
                "QR code timed out; generating a new one"
            }
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn render_qr(content: &str) -> anyhow::Result<String> {
    let code = qrcode::QrCode::new(content.as_bytes())
        .map_err(|error| anyhow::anyhow!("QR encode failed: {error}"))?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .build())
}

async fn fetch_devices(config: &mut Config) -> anyhow::Result<Vec<openuur::api::models::Device>> {
    auth::ensure_registered(config).await?;
    let api = api_client(config, true)?;
    Ok(api.device_list().await?)
}

fn print_install_report(report: &openuur::install::InstallReport) {
    println!(
        "installed {} service: {}",
        if report.system { "system" } else { "user" },
        report.unit_path.display()
    );
    if let Some(user) = &report.user {
        println!("account: {user}");
    }
    println!("binary: {}", report.binary.display());
    println!("config: {}", report.config.display());
    if report.enabled {
        println!(
            "service: enabled{}",
            if report.started { " and started" } else { "" }
        );
    } else {
        println!("service: unit written but could not be enabled");
        if report.system {
            println!(
                "  sudo systemctl daemon-reload && sudo systemctl enable --now openuurd.service"
            );
        } else {
            println!(
                "  systemctl --user daemon-reload && systemctl --user enable --now openuurd.service"
            );
        }
    }
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
}

fn print_devices(devices: &[openuur::api::models::Device], json: bool) {
    if json {
        let value = serde_json::to_value(devices).unwrap_or_default();
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return;
    }
    if devices.is_empty() {
        println!("no devices");
        return;
    }
    let id_width = devices
        .iter()
        .map(|d| d.device_id.chars().count())
        .max()
        .unwrap_or(4)
        .max(8);
    let name_width = devices
        .iter()
        .map(|d| d.device_name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<id_width$}  {:<name_width$}  {:<8}  ONLINE",
        "DEVICE ID", "NAME", "PLATFORM"
    );
    for device in devices {
        println!(
            "{:<id_width$}  {:<name_width$}  {:<8}  {}",
            device.device_id,
            openuur::util::truncate_chars(&device.device_name, name_width),
            device.display_platform(),
            if device.online { "yes" } else { "no" }
        );
    }
}

async fn rename_device(config: &mut Config, query: &str, name: &str) -> anyhow::Result<()> {
    auth::ensure_registered(config).await?;
    let api = api_client(config, true)?;
    let device = api.resolve_device(query).await?;
    let response = api.rename_device(&device.device_id, name).await?;
    let _ = response;
    if config.device_id.as_deref() == Some(device.device_id.as_str()) {
        config.device_name = Some(name.to_owned());
        config.save()?;
    }
    println!("renamed {} to {:?}", device.device_id, name);
    Ok(())
}

async fn set_wallpaper(
    config: &mut Config,
    query: &str,
    url: Option<String>,
    file: Option<PathBuf>,
    upload_url: Option<String>,
    dry_run: bool,
) -> anyhow::Result<()> {
    auth::ensure_registered(config).await?;
    let api = api_client(config, true)?;
    let device = api.resolve_device(query).await?;
    let wallpaper_url = match (url, file) {
        (Some(url), _) => url,
        (None, Some(file)) => {
            let endpoint = upload_url.or_else(|| config.upload_url.clone());
            api.upload_wallpaper(&file, endpoint.as_deref()).await?
        }
        (None, None) => anyhow::bail!("wallpaper needs --url or --file"),
    };
    if dry_run {
        println!("{wallpaper_url}");
        return Ok(());
    }
    api.set_wallpaper(Some(&device.device_id), &wallpaper_url)
        .await?;
    println!(
        "wallpaper updated for {} ({})",
        device.device_name, device.device_id
    );
    Ok(())
}

async fn run_local_terminal(config: &Config, args: &TermArgs) -> anyhow::Result<()> {
    let manager = SessionManager::new(None);
    let mut events = manager.subscribe();
    let (cols, rows) = terminal_size();
    let response = manager.open(&OpenRequest {
        session_id: None,
        name: None,
        shell: args.shell.clone().or_else(|| config.shell.clone()),
        cols: Some(cols),
        rows: Some(rows),
    })?;
    let session_id = response.session_id;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let _raw = if interactive {
        Some(RawModeGuard::enable())
    } else {
        None
    };

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buffer = [0u8; 4096];
        loop {
            match stdin.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.send(buffer[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut resize_tick = tokio::time::interval(Duration::from_millis(800));
    let mut last_size = (cols, rows);
    let mut exit_code = 0i32;
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(SessionEvent::Output { session_id: id, data }) if id == session_id => {
                        let mut stdout = tokio::io::stdout();
                        stdout.write_all(&data).await?;
                        stdout.flush().await?;
                    }
                    Ok(SessionEvent::Exited { session_id: id, exit_code: code }) if id == session_id => {
                        exit_code = code;
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            Some(bytes) = stdin_rx.recv() => {
                manager.write(session_id, &bytes)?;
            }
            _ = resize_tick.tick() => {
                let size = terminal_size();
                if size != last_size {
                    last_size = size;
                    manager.resize(session_id, size.0, size.1)?;
                }
            }
        }
    }
    if interactive {
        let _ = ct::disable_raw_mode();
        println!();
    }
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn terminal_size() -> (u16, u16) {
    ct::size().unwrap_or((120, 30))
}

struct RawModeGuard {
    _private: (),
}

impl RawModeGuard {
    fn enable() -> Self {
        let _ = ct::enable_raw_mode();
        Self { _private: () }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = ct::disable_raw_mode();
    }
}
