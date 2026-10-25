use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;

const SMC_PATH: &str = "/usr/local/libexec/battery/smc";
const ROOT_HELPER_PATH: &str = "/usr/local/libexec/battery/battery-root";
const LOOP_INTERVAL: Duration = Duration::from_secs(60);
const AGENT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

static SHUTDOWN_REQUESTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

#[derive(Parser)]
#[command(
    name = "battery",
    version,
    about = "Control and maintain MacBook battery charging",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// Show battery percentage, remaining time and SMC charging state.
    Status,
    /// Keep the battery near a percentage; use `stop` to disable maintenance.
    Maintain { level: String },
    /// Manually enable or disable battery charging.
    Charging { setting: String },
    /// Configure passwordless sudo for this user's supported SMC writes.
    Visudo,
    /// Enable or disable adapter input power.
    Adapter { setting: String },
    /// Charge until the selected percentage is reached.
    Charge { level: u8 },
    /// Discharge until the selected percentage is reached.
    Discharge { level: u8 },
    /// Print a machine-readable status row for the menu bar app.
    #[command(name = "status_csv", hide = true)]
    StatusCsv,
    /// Show command help.
    Help,
    /// Internal launchd maintenance worker.
    #[command(name = "maintain-synchronous", hide = true)]
    MaintainSynchronous { level: String },
    /// Internal launchd maintenance worker; restores the saved target.
    #[command(name = "maintain-recover", hide = true)]
    MaintainRecover,
    /// Root-only validated SMC write wrapper for dynamic Golden Gate limits.
    #[command(name = "golden-gate", hide = true)]
    GoldenGate {
        operation: String,
        upper: Option<String>,
        lower: Option<String>,
    },
}

#[derive(Clone)]
struct Paths {
    config: PathBuf,
    config_file: PathBuf,
    legacy_target: PathBuf,
    log: PathBuf,
    launch_agent: PathBuf,
}

impl Paths {
    fn new() -> Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
        let config = home.join(".battery");
        Ok(Self {
            config_file: config.join("config.toml"),
            legacy_target: config.join("maintain.percentage"),
            log: config.join("battery.log"),
            launch_agent: home.join("Library/LaunchAgents/battery.plist"),
            config,
        })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct AppConfig {
    maintenance: MaintenanceConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct MaintenanceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<u8>,
}

#[derive(Debug)]
struct BatteryStatus {
    percentage: String,
    remaining: String,
    charging: String,
    discharging: String,
    maintain_percentage: String,
    maintenance_problem: Option<String>,
}

#[derive(Debug)]
struct SmcReading {
    data_type: String,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct SmcCapabilities {
    golden_gate: bool,
    tahoe: bool,
    legacy: bool,
    adapter_chie: bool,
    adapter_ch0i: bool,
    adapter_ch0j: bool,
}

struct AdapterRestoreGuard {
    armed: bool,
}

impl AdapterRestoreGuard {
    fn new() -> Self {
        Self { armed: true }
    }

    fn restore(&mut self) -> Result<()> {
        enable_adapter_input()?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for AdapterRestoreGuard {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = enable_adapter_input()
        {
            tracing::error!(target: "battery", error = %error, "could not restore adapter input after discharge");
        }
    }
}

static SMC_CAPABILITIES: OnceLock<SmcCapabilities> = OnceLock::new();

fn main() {
    init_tracing();
    if let Err(error) = install_signal_handlers().and_then(|()| run()) {
        tracing::error!(target: "battery", error = %error, "command failed");
        std::process::exit(1);
    }
}

fn install_signal_handlers() -> Result<()> {
    let requested = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&requested))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&requested))?;
    SHUTDOWN_REQUESTED
        .set(requested)
        .map_err(|_| anyhow!("signal handlers were already installed"))
}

fn check_shutdown() -> Result<()> {
    if SHUTDOWN_REQUESTED
        .get()
        .is_some_and(|requested| requested.load(Ordering::Relaxed))
    {
        bail!("interrupted");
    }
    Ok(())
}

fn sleep_until_next_check() -> Result<()> {
    let deadline = Instant::now() + LOOP_INTERVAL;
    loop {
        check_shutdown()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
}

struct BatteryLogMakeWriter;

struct BatteryLogWriter {
    stderr: io::Stderr,
    file: Option<fs::File>,
}

impl<'a> MakeWriter<'a> for BatteryLogMakeWriter {
    type Writer = BatteryLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        let file = Paths::new().ok().and_then(|paths| {
            fs::create_dir_all(&paths.config).ok()?;
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(paths.log)
                .ok()
        });
        BatteryLogWriter {
            stderr: io::stderr(),
            file,
        }
    }
}

impl Write for BatteryLogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stderr.write_all(buffer)?;
        if let Some(file) = &mut self.file {
            let _ = file.write_all(buffer);
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        if let Some(file) = &mut self.file {
            let _ = file.flush();
        }
        Ok(())
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(false)
        .with_writer(BatteryLogMakeWriter)
        .try_init();
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
        Some(Action::Status) => show_status(),
        Some(Action::StatusCsv) => status_csv(),
        Some(Action::Help) => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
        Some(Action::Charging { setting }) => set_charging(&setting),
        Some(Action::Visudo) => install_sudoers(),
        Some(Action::Adapter { setting }) => set_adapter(&setting),
        Some(Action::Charge { level }) => charge_to(level),
        Some(Action::Discharge { level }) => discharge_to(level),
        Some(Action::Maintain { level }) => maintain(level),
        Some(Action::MaintainSynchronous { level }) => maintain_loop(&level),
        Some(Action::MaintainRecover) => {
            let paths = Paths::new()?;
            let target = load_config(&paths)?
                .maintenance
                .target
                .context("no saved maintenance target in ~/.battery/config.toml")?;
            maintain_loop(&target.to_string())
        }
        Some(Action::GoldenGate {
            operation,
            upper,
            lower,
        }) => golden_gate_root_action(&operation, upper.as_deref(), lower.as_deref()),
    }
}

fn paths() -> Result<Paths> {
    let paths = Paths::new()?;
    fs::create_dir_all(&paths.config).context("cannot create ~/.battery")?;
    Ok(paths)
}

fn load_config(paths: &Paths) -> Result<AppConfig> {
    if paths.config_file.exists() {
        let contents = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("cannot read {}", paths.config_file.display()))?;
        let config: AppConfig = toml::from_str(&contents)
            .with_context(|| format!("invalid TOML in {}", paths.config_file.display()))?;
        if config
            .maintenance
            .target
            .is_some_and(|target| !(1..=100).contains(&target))
        {
            bail!("maintenance.target in config.toml must be between 1 and 100");
        }
        return Ok(config);
    }

    // Migrate the original plain-text target on first use.
    if paths.legacy_target.exists() {
        let raw = fs::read_to_string(&paths.legacy_target)
            .context("cannot read legacy maintenance target")?;
        let target: u8 = raw
            .trim()
            .parse()
            .context("invalid legacy maintenance target")?;
        if !(1..=100).contains(&target) {
            bail!("legacy maintenance target must be between 1 and 100");
        }
        let config = AppConfig {
            maintenance: MaintenanceConfig {
                target: Some(target),
            },
        };
        save_config(paths, &config)?;
        fs::remove_file(&paths.legacy_target).context("cannot remove migrated target file")?;
        return Ok(config);
    }

    Ok(AppConfig::default())
}

fn save_config(paths: &Paths, config: &AppConfig) -> Result<()> {
    let contents = toml::to_string_pretty(config).context("cannot serialize TOML config")?;
    let temporary = paths.config_file.with_extension("toml.tmp");
    fs::write(&temporary, contents)
        .with_context(|| format!("cannot write temporary config {}", temporary.display()))?;
    fs::rename(&temporary, &paths.config_file)
        .with_context(|| format!("cannot replace config {}", paths.config_file.display()))
}

fn install_sudoers() -> Result<()> {
    let paths = paths()?;
    let user_output = Command::new("/usr/bin/id")
        .arg("-un")
        .output()
        .context("cannot determine current user name")?;
    if !user_output.status.success() {
        bail!("id -un failed");
    }
    let username = String::from_utf8_lossy(&user_output.stdout)
        .trim()
        .to_owned();
    if username.is_empty()
        || !username
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        bail!("current user name cannot be safely represented in sudoers");
    }

    let executable = std::env::current_exe().context("cannot locate battery executable")?;
    let smc_path = sudoers_command_path(Path::new(SMC_PATH))?;
    let helper_path = sudoers_command_path(Path::new(ROOT_HELPER_PATH))?;
    let rules = format!(
        "# Managed by battery visudo. Edit through `battery visudo`.\n\
Cmnd_Alias BATTERY_CHARGING_ON = {smc_path} -k CH0B -w 00, {smc_path} -k CH0C -w 00, {smc_path} -k CHTE -w 00000000\n\
Cmnd_Alias BATTERY_CHARGING_OFF = {smc_path} -k CH0B -w 02, {smc_path} -k CH0C -w 02, {smc_path} -k CHTE -w 01000000\n\
Cmnd_Alias BATTERY_ADAPTER_ON = {smc_path} -k CH0I -w 01, {smc_path} -k CHIE -w 08, {smc_path} -k CH0J -w 01\n\
Cmnd_Alias BATTERY_ADAPTER_OFF = {smc_path} -k CH0I -w 00, {smc_path} -k CHIE -w 00, {smc_path} -k CH0J -w 00\n\
{username} ALL=(root) NOPASSWD: BATTERY_CHARGING_ON, BATTERY_CHARGING_OFF, BATTERY_ADAPTER_ON, BATTERY_ADAPTER_OFF\n\
# Dynamic Golden Gate values are passed through a root-only Rust wrapper that validates each argument.\n\
{username} ALL=(root) NOPASSWD: {helper_path} golden-gate *\n"
    );
    let validation = Command::new("sudo")
        .arg("-v")
        .stdin(Stdio::inherit())
        .status()
        .context("could not run sudo to install sudoers rules")?;
    if !validation.success() {
        bail!("sudo authentication failed; sudoers was not changed");
    }

    validate_root_owned_directory(Path::new(ROOT_HELPER_PATH).parent().unwrap())?;
    validate_root_owned_executable(Path::new(SMC_PATH))?;
    sudo_checked(
        vec![
            "install".into(),
            "-o".into(),
            "root".into(),
            "-g".into(),
            "wheel".into(),
            "-m".into(),
            "0755".into(),
            executable.as_os_str().to_owned(),
            ROOT_HELPER_PATH.into(),
        ],
        "install root-owned SMC helper",
    )?;
    validate_root_owned_executable(Path::new(ROOT_HELPER_PATH))?;

    let temporary = paths.config.join("sudoers.tmp");
    fs::write(&temporary, rules).context("cannot write temporary sudoers file")?;

    sudo_checked(
        vec![
            "visudo".into(),
            "-c".into(),
            "-f".into(),
            temporary.as_os_str().to_owned(),
        ],
        "validate temporary sudoers file",
    )?;
    sudo_checked(
        vec!["mkdir".into(), "-p".into(), "/private/etc/sudoers.d".into()],
        "create sudoers directory",
    )?;
    sudo_checked(
        vec![
            "install".into(),
            "-o".into(),
            "root".into(),
            "-g".into(),
            "wheel".into(),
            "-m".into(),
            "0440".into(),
            temporary.as_os_str().to_owned(),
            "/private/etc/sudoers.d/battery".into(),
        ],
        "install sudoers rules",
    )?;
    sudo_checked(
        vec!["visudo".into(), "-c".into()],
        "validate installed sudoers configuration",
    )?;
    let _ = fs::remove_file(&temporary);
    println!("Installed and validated sudoers rules for {username}.");
    Ok(())
}

fn sudoers_command_path(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .context("sudoers executable path is not valid UTF-8")?;
    if !path.starts_with('/')
        || path
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, ',' | '#' | '\\' | '*' | '?' | '[' | ']'))
    {
        bail!("executable path cannot be safely represented in sudoers: {path}");
    }
    Ok(path.to_owned())
}

fn validate_root_owned_directory(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        let metadata = fs::metadata(ancestor)
            .with_context(|| format!("cannot inspect {}", ancestor.display()))?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!(
                "refusing passwordless sudo because {} is not a root-owned, non-writable directory",
                ancestor.display()
            );
        }
    }
    Ok(())
}

fn validate_root_owned_executable(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("cannot inspect executable {}", path.display()))?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        bail!(
            "refusing passwordless sudo because {} is not a root-owned, non-writable executable",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        validate_root_owned_directory(parent)?;
    }
    Ok(())
}

fn sudo_checked(args: Vec<OsString>, description: &str) -> Result<()> {
    let output = Command::new("sudo")
        .args(args)
        .stdin(Stdio::inherit())
        .output()
        .with_context(|| format!("could not {description}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    bail!(
        "could not {description}: {}",
        if stderr.is_empty() { stdout } else { stderr }
    )
}

fn log(message: &str) {
    tracing::info!(target: "battery", message = %message);
}

fn smc_read(key: &str) -> Result<SmcReading> {
    let output = Command::new(SMC_PATH)
        .args(["-k", key, "-r"])
        .output()
        .with_context(|| format!("could not execute {SMC_PATH}"))?;
    if !output.status.success() {
        bail!(
            "smc read {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    match parse_smc_value(&text) {
        Ok(reading) => {
            tracing::debug!(target: "battery", key, bytes = ?reading.bytes, "SMC key read");
            Ok(reading)
        }
        Err(error) => {
            tracing::debug!(target: "battery", key, output = %text.trim(), "SMC key has no readable data");
            Err(error).with_context(|| format!("could not read SMC key {key}: {}", text.trim()))
        }
    }
}

fn parse_smc_value(output: &str) -> Result<SmcReading> {
    let line = output
        .lines()
        .next()
        .ok_or_else(|| anyhow!("empty smc output"))?;
    let (type_start, type_end) = (line.rfind('['), line.rfind(']'));
    let data_type = match (type_start, type_end) {
        (Some(start), Some(end)) if start < end => line[start + 1..end].trim().to_ascii_lowercase(),
        _ => String::new(),
    };

    let byte_text = if let Some(start) = line.find("(bytes") {
        line[start + "(bytes".len()..]
            .split_once(')')
            .map(|(bytes, _)| bytes.trim())
    } else if !data_type.is_empty() && data_type != "hex_" {
        type_start
            .zip(type_end)
            .map(|(start, end)| line[start + 1..end].trim())
    } else {
        line.rsplit_once(':').map(|(_, value)| value.trim())
    }
    .ok_or_else(|| anyhow!("SMC returned no data"))?;
    let bytes = parse_hex_bytes(byte_text)?;
    if bytes.is_empty() {
        bail!("SMC returned no data")
    }
    Ok(SmcReading { data_type, bytes })
}

fn parse_hex_bytes(value: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for token in value.split_whitespace() {
        let token = token.trim_matches(|c: char| !c.is_ascii_hexdigit());
        if token.is_empty() {
            continue;
        }
        if token.len() != 2 {
            bail!("invalid SMC byte {token:?}");
        }
        bytes.push(u8::from_str_radix(token, 16).context("invalid SMC hex byte")?);
    }
    Ok(bytes)
}

fn smc_write(key: &str, bytes: &[u8]) -> Result<()> {
    let value = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    tracing::debug!(target: "battery", key, value, "writing SMC key");
    let mut command = if running_as_root()? {
        Command::new(SMC_PATH)
    } else {
        let mut command = Command::new("sudo");
        command.arg(SMC_PATH);
        command
    };
    let output = command
        .args(["-k", key, "-w", &value])
        .stdin(Stdio::inherit())
        .output()
        .with_context(|| format!("could not run sudo {SMC_PATH}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        bail!(
            "writing SMC key {key} failed: {}",
            if stderr.is_empty() { stdout } else { stderr }
        );
    }
    Ok(())
}

fn running_as_root() -> Result<bool> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .context("cannot determine effective user ID")?;
    if !output.status.success() {
        bail!("id -u failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "0")
}

fn smc_write_verified(key: &str, bytes: &[u8]) -> Result<()> {
    smc_write(key, bytes)?;
    let actual = smc_read(key)?;
    if actual.bytes != bytes {
        bail!(
            "SMC verification failed for {key}: wrote {bytes:02x?}, read {:02x?}",
            actual.bytes
        );
    }
    Ok(())
}

fn probe_key(key: &str, expected_type: Option<&str>, expected_len: usize) -> bool {
    smc_read(key).is_ok_and(|reading| {
        reading.bytes.len() == expected_len
            && expected_type.is_none_or(|kind| reading.data_type == kind)
    })
}

fn smc_capabilities() -> &'static SmcCapabilities {
    SMC_CAPABILITIES.get_or_init(|| {
        let capabilities = SmcCapabilities {
            golden_gate: probe_key("bfD0", Some("ui32"), 4)
                && probe_key("bfE0", Some("ui32"), 4)
                && probe_key("bfF0", Some("ui8"), 1),
            tahoe: probe_key("CHTE", None, 4),
            legacy: probe_key("CH0B", None, 1) && probe_key("CH0C", None, 1),
            adapter_chie: probe_key("CHIE", None, 1),
            adapter_ch0i: probe_key("CH0I", None, 1),
            adapter_ch0j: probe_key("CH0J", None, 1),
        };
        if !running_as_root().unwrap_or(false) {
            log(&format!(
                "SMC capabilities: golden_gate={} tahoe={} legacy={} CHIE={} CH0I={} CH0J={}",
                capabilities.golden_gate,
                capabilities.tahoe,
                capabilities.legacy,
                capabilities.adapter_chie,
                capabilities.adapter_ch0i,
                capabilities.adapter_ch0j
            ));
        }
        capabilities
    })
}

fn enable_charging_smc() -> Result<()> {
    let capabilities = smc_capabilities();
    if capabilities.golden_gate {
        run_golden_gate_privileged("reset", None, None)
    } else if capabilities.tahoe {
        smc_write_verified("CHTE", &[0x00, 0x00, 0x00, 0x00])
    } else if capabilities.legacy {
        smc_write_verified("CH0B", &[0x00])?;
        smc_write_verified("CH0C", &[0x00])
    } else {
        bail!("no supported SMC charging control keys were found")
    }
}

fn write_golden_gate_limit_raw(upper: u8, lower: u8) -> Result<()> {
    if upper < 2 || upper > 100 || lower == 0 || lower >= upper {
        bail!("invalid Golden Gate charging range: lower={lower}, upper={upper}");
    }
    let upper_bytes = [upper, 0, 0, 0];
    let lower_bytes = [lower, 0, 0, 0];
    let update = (|| {
        smc_write_verified("bfF0", &[0x00])?;
        smc_write_verified("bfD0", &upper_bytes)?;
        smc_write_verified("bfE0", &lower_bytes)?;
        smc_write_verified("bfF0", &[0x02])
    })();
    if update.is_err() {
        let _ = smc_write_verified("bfF0", &[0x00]);
    }
    update
}

fn golden_gate_root_action(
    operation: &str,
    upper: Option<&str>,
    lower: Option<&str>,
) -> Result<()> {
    if !running_as_root()? {
        bail!("Golden Gate SMC operations must run as root");
    }
    if !smc_capabilities().golden_gate {
        bail!("Golden Gate SMC keys are unavailable on this machine");
    }
    match (operation, upper, lower) {
        ("reset", None, None) => smc_write_verified("bfF0", &[0x00]),
        ("set", Some(upper), Some(lower)) => {
            let upper = upper
                .parse::<u8>()
                .context("upper limit must be an integer")?;
            let lower = lower
                .parse::<u8>()
                .context("lower limit must be an integer")?;
            write_golden_gate_limit_raw(upper, lower)
        }
        _ => bail!("invalid Golden Gate operation"),
    }
}

fn run_golden_gate_privileged(operation: &str, upper: Option<u8>, lower: Option<u8>) -> Result<()> {
    if running_as_root()? {
        let upper = upper.map(|value| value.to_string());
        let lower = lower.map(|value| value.to_string());
        return golden_gate_root_action(operation, upper.as_deref(), lower.as_deref());
    }
    let mut command = Command::new("sudo");
    command
        .arg(ROOT_HELPER_PATH)
        .arg("golden-gate")
        .arg(operation);
    if let Some(upper) = upper {
        command.arg(upper.to_string());
    }
    if let Some(lower) = lower {
        command.arg(lower.to_string());
    }
    let output = command
        .stdin(Stdio::inherit())
        .output()
        .context("could not run privileged Golden Gate operation")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        bail!(
            "Golden Gate operation failed: {}",
            if stderr.is_empty() { stdout } else { stderr }
        );
    }
    Ok(())
}

fn write_golden_gate_limit(upper: u8, lower: u8) -> Result<()> {
    run_golden_gate_privileged("set", Some(upper), Some(lower))
}

fn enable_charging() -> Result<()> {
    log("Enabling battery charging");
    enable_charging_smc()
}

fn disable_charging() -> Result<()> {
    log("Disabling battery charging");
    let capabilities = smc_capabilities();
    if capabilities.golden_gate {
        let (raw, _) = battery_reading()?;
        let current: u8 = raw.parse().context("invalid battery percentage")?;
        let upper = current.saturating_sub(2).max(3);
        let lower = upper.saturating_sub(2);
        write_golden_gate_limit(upper, lower)
    } else if capabilities.tahoe {
        smc_write_verified("CHTE", &[0x01, 0x00, 0x00, 0x00])
    } else if capabilities.legacy {
        smc_write_verified("CH0B", &[0x02])?;
        smc_write_verified("CH0C", &[0x02])
    } else {
        bail!("no supported SMC charging control keys were found")
    }
}

fn disable_adapter_input() -> Result<()> {
    log("Disabling adapter input; battery will supply power");
    let capabilities = smc_capabilities();
    let mut supported = false;
    if capabilities.adapter_ch0i {
        smc_write("CH0I", &[0x01])?;
        supported = true;
    }
    if capabilities.adapter_chie {
        smc_write("CHIE", &[0x08])?;
        supported = true;
    }
    if capabilities.adapter_ch0j {
        // On newer firmware, CH0J reads back the encoded CHIE state (for example 0x28)
        // rather than the 0x01 command value, so success is determined by the SMC write status.
        smc_write("CH0J", &[0x01])?;
        supported = true;
    }
    if supported {
        Ok(())
    } else {
        bail!("no supported SMC adapter control keys were found")
    }
}

fn enable_adapter_input() -> Result<()> {
    log("Enabling adapter input");
    let capabilities = smc_capabilities();
    let mut supported = false;
    if capabilities.adapter_ch0i {
        smc_write("CH0I", &[0x00])?;
        supported = true;
    }
    if capabilities.adapter_chie {
        smc_write("CHIE", &[0x00])?;
        supported = true;
    }
    if capabilities.adapter_ch0j {
        smc_write("CH0J", &[0x00])?;
        supported = true;
    }
    if supported {
        Ok(())
    } else {
        bail!("no supported SMC adapter control keys were found")
    }
}

fn charging_status() -> String {
    let capabilities = smc_capabilities();
    let (key, enabled, disabled) = if capabilities.golden_gate {
        ("bfF0", vec![0x00], vec![0x02])
    } else if capabilities.tahoe {
        (
            "CHTE",
            vec![0x00, 0x00, 0x00, 0x00],
            vec![0x01, 0x00, 0x00, 0x00],
        )
    } else if capabilities.legacy {
        ("CH0B", vec![0x00], vec![0x02])
    } else {
        return "unknown".to_owned();
    };
    match smc_read(key).map(|reading| reading.bytes) {
        Ok(value) if value == enabled => "enabled".to_owned(),
        Ok(value) if value == disabled => "disabled".to_owned(),
        _ => "unknown".to_owned(),
    }
}

fn discharging_status() -> String {
    let capabilities = smc_capabilities();
    let key = if capabilities.adapter_chie {
        "CHIE"
    } else if capabilities.adapter_ch0i {
        "CH0I"
    } else if capabilities.adapter_ch0j {
        "CH0J"
    } else {
        return "unknown".to_owned();
    };
    match smc_read(key).map(|reading| reading.bytes) {
        Ok(value) if value == [0x00] => "not discharging".to_owned(),
        Ok(_) => "discharging".to_owned(),
        Err(_) => "unknown".to_owned(),
    }
}

fn battery_reading() -> Result<(String, String)> {
    let output = Command::new("pmset")
        .args(["-g", "batt"])
        .output()
        .context("could not execute pmset -g batt (macOS only)")?;
    if !output.status.success() {
        bail!(
            "pmset -g batt failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rev()
        .find(|line| line.contains('%'))
        .ok_or_else(|| anyhow!("pmset did not report a battery percentage: {}", text.trim()))?;
    let percentage = line
        .split_whitespace()
        .find_map(|part| {
            let value = part.trim_matches(|c: char| !c.is_ascii_digit());
            if part.contains('%') && !value.is_empty() {
                Some(value.to_owned())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("could not parse battery percentage from: {}", line.trim()))?;
    let remaining = line
        .split(';')
        .find_map(|part| {
            let part = part.trim();
            if part.contains("remaining") {
                Some(
                    part.split_whitespace()
                        .take_while(|word| *word != "remaining")
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_owned());
    Ok((percentage, remaining))
}

fn current_status() -> Result<BatteryStatus> {
    let (percentage, remaining) = battery_reading()?;
    let (maintain_percentage, maintenance_problem) = match load_config(&paths()?)?
        .maintenance
        .target
    {
        Some(target) => match launch_agent_state() {
            Ok(Some(state)) if state == "running" => (target.to_string(), None),
            Ok(state) => (
                String::new(),
                Some(format!(
                    "Maintenance target {target}% is configured, but the agent is {}",
                    state.unwrap_or_else(|| "not loaded".to_owned())
                )),
            ),
            Err(error) => (
                String::new(),
                Some(format!(
                    "Maintenance target {target}% is configured, but the agent could not be checked: {error}"
                )),
            ),
        },
        None => (String::new(), None),
    };
    Ok(BatteryStatus {
        percentage,
        remaining,
        charging: charging_status(),
        discharging: discharging_status(),
        maintain_percentage,
        maintenance_problem,
    })
}

fn show_status() -> Result<()> {
    let status = current_status()?;
    log(&format!(
        "Battery at {}% ({} remaining), SMC charging {}",
        status.percentage, status.remaining, status.charging
    ));
    if !status.maintain_percentage.is_empty() {
        println!(
            "Your battery is currently being maintained at {}%",
            status.maintain_percentage
        );
    }
    if let Some(problem) = status.maintenance_problem {
        eprintln!("{problem}");
    }
    if status.discharging == "discharging" {
        println!("Adapter input is disabled; battery is discharging");
    }
    Ok(())
}

fn status_csv() -> Result<()> {
    let status = current_status()?;
    println!(
        "{},{},{},{},{}",
        status.percentage,
        status.remaining,
        status.charging,
        status.discharging,
        status.maintain_percentage
    );
    Ok(())
}

fn validate_setting(setting: &str) -> Result<bool> {
    match setting {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => bail!("setting must be `on` or `off`"),
    }
}

fn set_charging(setting: &str) -> Result<()> {
    let enabled = validate_setting(setting)?;
    stop_maintenance()?;
    if enabled {
        enable_charging()
    } else {
        disable_charging()
    }
}

fn set_adapter(setting: &str) -> Result<()> {
    let enabled = validate_setting(setting)?;
    clear_maintenance()?;
    if enabled {
        // Adapter control is independent of the battery charging limit.
        enable_adapter_input()?;
    } else {
        disable_adapter_input()?;
    }
    show_status()
}

fn validate_level(level: u8) -> Result<u8> {
    if (1..=100).contains(&level) {
        Ok(level)
    } else {
        bail!("percentage must be between 1 and 100")
    }
}

fn wait_for_level(level: u8, charging: bool) -> Result<()> {
    let level = validate_level(level)?;
    check_shutdown()?;
    let (initial, _) = battery_reading()?;
    log(&format!(
        "{} to {level}% from {initial}%",
        if charging { "Charging" } else { "Discharging" }
    ));
    let mut adapter_restore = if charging {
        None
    } else {
        // Arm before the first write so partial SMC updates are also restored.
        Some(AdapterRestoreGuard::new())
    };
    if charging {
        enable_adapter_input()?;
        enable_charging()?;
    } else {
        disable_adapter_input()?;
    }
    loop {
        check_shutdown()?;
        let (raw, _) = battery_reading()?;
        let current: u8 = raw.parse().context("invalid battery percentage")?;
        log(&format!("Battery at {current}% (target {level}%)"));
        if (charging && current >= level) || (!charging && current <= level) {
            break;
        }
        sleep_until_next_check()?;
    }
    if charging {
        disable_charging()?;
    } else {
        adapter_restore.as_mut().unwrap().restore()?;
    }
    log(&format!(
        "{} completed",
        if charging { "Charging" } else { "Discharging" }
    ));
    Ok(())
}

fn charge_to(level: u8) -> Result<()> {
    stop_maintenance()?;
    wait_for_level(level, true)
}

fn discharge_to(level: u8) -> Result<()> {
    stop_maintenance()?;
    wait_for_level(level, false)
}

fn parse_target(level: &str) -> Result<u8> {
    let parsed: u8 = level
        .parse()
        .context("target must be a percentage from 1 to 100")?;
    validate_level(parsed)
}

fn maintain(level: String) -> Result<()> {
    if level == "stop" {
        return stop_maintenance();
    }
    let target = parse_target(&level)?;
    let paths = paths()?;
    unload_launch_agent()?;
    let mut config = load_config(&paths)?;
    config.maintenance.target = Some(target);
    save_config(&paths, &config)?;
    install_launch_agent(&paths)?;
    bootstrap_launch_agent(&paths.launch_agent)?;
    wait_for_agent_running()?;
    log(&format!("Maintaining battery at {target}%"));
    Ok(())
}

fn maintain_loop(level: &str) -> Result<()> {
    let target = if level == "recover" {
        let paths = paths()?;
        load_config(&paths)?
            .maintenance
            .target
            .context("no saved maintenance target in ~/.battery/config.toml")?
    } else {
        parse_target(level)?
    };
    let _adapter_restore = AdapterRestoreGuard::new();
    // A previous worker may have exited while adapter input was disabled.
    enable_adapter_input()?;
    let (initial, _) = battery_reading()?;
    if initial.parse::<u8>().unwrap_or(0) > target {
        log(&format!(
            "Discharging to {target}% before starting maintenance"
        ));
        wait_for_level(target, false)?;
    }
    loop {
        check_shutdown()?;
        if discharging_status() != "not discharging" {
            enable_adapter_input()?;
        }
        let (raw, _) = battery_reading()?;
        let current: u8 = raw.parse().context("invalid battery percentage")?;
        let charging = charging_status();
        if current >= target && charging == "enabled" {
            log(&format!("Charge at or above {target}%; disabling charging"));
            disable_charging()?;
        } else if current < target && charging == "disabled" {
            log(&format!("Charge below {target}%; enabling charging"));
            enable_charging()?;
        }
        sleep_until_next_check()?;
    }
}

fn clear_maintenance() -> Result<()> {
    let paths = paths()?;
    log("Stopping battery maintenance");
    unload_launch_agent()?;
    let mut config = load_config(&paths)?;
    config.maintenance.target = None;
    save_config(&paths, &config)?;
    if paths.launch_agent.exists() {
        fs::remove_file(&paths.launch_agent).context("cannot remove launch agent plist")?;
    }
    Ok(())
}

fn stop_maintenance() -> Result<()> {
    clear_maintenance()?;
    log("Enabling battery charging");
    enable_charging()?;
    show_status()
}

fn install_launch_agent(paths: &Paths) -> Result<()> {
    let executable = std::env::current_exe().context("cannot locate battery executable")?;
    let parent = paths
        .launch_agent
        .parent()
        .context("invalid LaunchAgents path")?;
    fs::create_dir_all(parent).context("cannot create LaunchAgents directory")?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.battery.app</string>
<key>ProgramArguments</key><array><string>{}</string><string>maintain-recover</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>
"#,
        xml_escape(&executable.to_string_lossy())
    );
    fs::write(&paths.launch_agent, plist).context("cannot write launch agent")
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn launch_domain() -> Result<String> {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .context("cannot determine current user ID")?;
    if !uid.status.success() {
        bail!("id -u failed");
    }
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_owned();
    Ok(format!("gui/{uid}"))
}

fn launch_agent_state() -> Result<Option<String>> {
    let service = format!("{}/com.battery.app", launch_domain()?);
    let output = Command::new("launchctl")
        .args(["print", &service])
        .output()
        .context("could not inspect battery launch agent")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Could not find service") {
            return Ok(None);
        }
        bail!("launchctl print failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let state = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("state = "))
        .context("launchctl print did not report agent state")?;
    Ok(Some(state.to_owned()))
}

fn launchctl(args: &[&str], plist: &Path) -> Result<()> {
    let domain = launch_domain()?;
    let output = Command::new("launchctl")
        .args(args)
        .arg(domain)
        .arg(plist)
        .output()
        .context("could not execute launchctl")?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "launchctl {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn bootstrap_launch_agent(plist: &Path) -> Result<()> {
    launchctl(&["bootstrap"], plist)
}

fn wait_for_agent_running() -> Result<()> {
    thread::sleep(Duration::from_millis(750));
    let deadline = Instant::now() + AGENT_STOP_TIMEOUT;
    loop {
        match launch_agent_state()? {
            Some(state) if state == "running" => return Ok(()),
            state if Instant::now() >= deadline => {
                bail!(
                    "battery maintenance agent did not start (state: {}); check ~/.battery/battery.log",
                    state.unwrap_or_else(|| "not loaded".to_owned())
                );
            }
            _ => thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn unload_launch_agent() -> Result<()> {
    if launch_agent_state()?.is_none() {
        return Ok(());
    }
    let service = format!("{}/com.battery.app", launch_domain()?);
    let output = Command::new("launchctl")
        .args(["bootout", &service])
        .output()
        .context("could not stop battery launch agent")?;
    if !output.status.success() {
        if launch_agent_state()?.is_some() {
            bail!(
                "launchctl bootout failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(());
    }
    let deadline = Instant::now() + AGENT_STOP_TIMEOUT;
    while launch_agent_state()?.is_some() {
        if Instant::now() >= deadline {
            bail!("battery maintenance agent did not stop within 10 seconds");
        }
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}
