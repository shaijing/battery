# battery

Rust CLI for the core battery controls from the original `battery` project. It uses macOS `pmset -g batt` for battery readings and `/usr/local/libexec/battery/smc` for SMC access.

## Commands

```text
battery status
battery charging on|off
battery adapter on|off
battery charge 1..100
battery discharge 1..100
battery maintain 1..100|stop
```

`maintain` saves the target in `~/.battery/config.toml` and registers a per-user LaunchAgent at `~/Library/LaunchAgents/battery.plist`, so the limit is restored at login. Existing `~/.battery/maintain.percentage` state is migrated to TOML the first time it is read. SMC writes use `sudo`; configure sudoers if a non-interactive maintenance worker needs passwordless access to the SMC write operations.

`status_csv` is also available for the original menu bar app integration and prints percentage, remaining time, charging state, discharging state, and maintenance target as comma-separated fields.

Charging control probes SMC key availability at runtime instead of assuming a macOS version: it selects Golden Gate (`bfD0`/`bfE0`/`bfF0`), Tahoe (`CHTE`), or legacy (`CH0B`/`CH0C`) keys according to the keys and data widths exposed by the machine firmware. Adapter control similarly detects `CHIE`, `CH0I`, and `CH0J`. Unsupported SMC writes fail visibly; key availability can depend on firmware and hardware, even on the same macOS release.

The utility targets Apple Silicon macOS. Build with `cargo build --release` and make sure the configured SMC executable is installed and executable.
