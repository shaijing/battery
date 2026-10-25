use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

const SMC_PATH: &str = "/usr/local/libexec/battery/smc";
const LOOP_INTERVAL: Duration = Duration::from_secs(60);

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
    /// Enable or disable adapter input (discharging control).
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

static SMC_CAPABILITIES: OnceLock<SmcCapabilities> = OnceLock::new();

fn main() {
    if let Err(error) = run() {
        eprintln!("battery: {error:#}");
        std::process::exit(1);
    }
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

fn log(message: &str) {
    let stamp = Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_else(|| "battery".to_owned());
    let line = format!("{stamp} - {message}");
    println!("{line}");
    if let Ok(paths) = paths() {
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.log)
            .and_then(|mut file| {
                std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes())
            });
    }
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
    parse_smc_value(&text).with_context(|| format!("could not read SMC key {key}: {}", text.trim()))
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
    let output = Command::new("sudo")
        .args([SMC_PATH, "-k", key, "-w", &value])
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
        log(&format!(
            "SMC capabilities: golden_gate={} tahoe={} legacy={} CHIE={} CH0I={} CH0J={}",
            capabilities.golden_gate,
            capabilities.tahoe,
            capabilities.legacy,
            capabilities.adapter_chie,
            capabilities.adapter_ch0i,
            capabilities.adapter_ch0j
        ));
        capabilities
    })
}

fn enable_charging_smc() -> Result<()> {
    let capabilities = smc_capabilities();
    if capabilities.golden_gate {
        smc_write_verified("bfF0", &[0x00])
    } else if capabilities.tahoe {
        smc_write_verified("CHTE", &[0x00, 0x00, 0x00, 0x00])
    } else if capabilities.legacy {
        smc_write_verified("CH0B", &[0x00])?;
        smc_write_verified("CH0C", &[0x00])
    } else {
        bail!("no supported SMC charging control keys were found")
    }
}

fn write_golden_gate_limit(upper: u8, lower: u8) -> Result<()> {
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

fn enable_charging() -> Result<()> {
    log("Enabling battery charging");
    enable_charging_smc()?;
    disable_discharging()
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

fn enable_discharging() -> Result<()> {
    log("Enabling battery discharging");
    let capabilities = smc_capabilities();
    let mut supported = false;
    if capabilities.adapter_ch0i {
        smc_write_verified("CH0I", &[0x01])?;
        supported = true;
    }
    if capabilities.adapter_chie {
        smc_write_verified("CHIE", &[0x08])?;
        supported = true;
    }
    if capabilities.adapter_ch0j {
        smc_write_verified("CH0J", &[0x01])?;
        supported = true;
    }
    if supported {
        Ok(())
    } else {
        bail!("no supported SMC adapter control keys were found")
    }
}

fn disable_discharging() -> Result<()> {
    log("Disabling battery discharging");
    let capabilities = smc_capabilities();
    let mut supported = false;
    if capabilities.adapter_ch0i {
        smc_write_verified("CH0I", &[0x00])?;
        supported = true;
    }
    if capabilities.adapter_chie {
        smc_write_verified("CHIE", &[0x00])?;
        supported = true;
    }
    if capabilities.adapter_ch0j {
        smc_write_verified("CH0J", &[0x00])?;
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
    let maintain_percentage = load_config(&paths()?)?
        .maintenance
        .target
        .map(|target| target.to_string())
        .unwrap_or_default();
    Ok(BatteryStatus {
        percentage,
        remaining,
        charging: charging_status(),
        discharging: discharging_status(),
        maintain_percentage,
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
    stop_maintenance()?;
    if enabled {
        enable_discharging()
    } else {
        disable_discharging()
    }
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
    let (initial, _) = battery_reading()?;
    log(&format!(
        "{} to {level}% from {initial}%",
        if charging { "Charging" } else { "Discharging" }
    ));
    if charging {
        disable_discharging()?;
        enable_charging()?;
    } else {
        enable_discharging()?;
    }
    loop {
        let (raw, _) = battery_reading()?;
        let current: u8 = raw.parse().context("invalid battery percentage")?;
        log(&format!("Battery at {current}% (target {level}%)"));
        if (charging && current >= level) || (!charging && current <= level) {
            break;
        }
        thread::sleep(LOOP_INTERVAL);
    }
    if charging {
        disable_charging()?;
    } else {
        disable_discharging()?;
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
    unload_launch_agent(&paths.launch_agent);
    let mut config = load_config(&paths)?;
    config.maintenance.target = Some(target);
    save_config(&paths, &config)?;
    install_launch_agent(&paths)?;
    bootstrap_launch_agent(&paths.launch_agent)?;
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
    log(&format!(
        "Discharging to {target}% before starting maintenance"
    ));
    let (initial, _) = battery_reading()?;
    if initial.parse::<u8>().unwrap_or(0) > target {
        wait_for_level(target, false)?;
    }
    loop {
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
        thread::sleep(LOOP_INTERVAL);
    }
}

fn stop_maintenance() -> Result<()> {
    let paths = paths()?;
    log("Stopping battery maintenance and enabling charging");
    unload_launch_agent(&paths.launch_agent);
    let mut config = load_config(&paths)?;
    config.maintenance.target = None;
    save_config(&paths, &config)?;
    let _ = fs::remove_file(&paths.launch_agent);
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
<key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string>
<key>RunAtLoad</key><true/>
</dict></plist>
"#,
        xml_escape(&executable.to_string_lossy()),
        xml_escape(&paths.log.to_string_lossy()),
        xml_escape(&paths.log.to_string_lossy())
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

fn launchctl(args: &[&str], plist: &Path) -> Result<()> {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .context("cannot determine current user ID")?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_owned();
    let domain = format!("gui/{uid}");
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

fn unload_launch_agent(plist: &Path) {
    let _ = launchctl(&["bootout"], plist);
}
