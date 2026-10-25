# battery

Rust CLI for the core battery controls from the original `battery` project. It uses macOS `pmset -g batt` for battery readings and `/usr/local/libexec/battery/smc` for SMC access.

## Commands

```text
battery status
battery visudo
battery charging on|off
battery adapter on|off
battery charge 1..100
battery discharge 1..100
battery maintain 1..100|stop
```

Run `battery visudo` once to install and validate a sudoers file for the current user. It allows only the supported fixed SMC writes plus the validated Golden Gate wrapper, so the LaunchAgent can manage charging without a password prompt. `maintain` saves its target in `~/.battery/config.toml` and registers a per-user LaunchAgent at `~/Library/LaunchAgents/battery.plist`, so the limit is restored at login. Existing `~/.battery/maintain.percentage` state is migrated to TOML the first time it is read.

`maintain` checks that the LaunchAgent is running before reporting success. `status` reports a saved target as active only while the agent is running. Manual `charge` and `discharge` commands stop maintenance first. The agent restores adapter input when it starts and when it exits after a handled error, SIGINT, or SIGTERM.

`status_csv` is also available for the original menu bar app integration and prints percentage, remaining time, charging state, discharging state, and maintenance target as comma-separated fields.

Logs use `tracing`, go to stderr and `~/.battery/battery.log`, and default to the `info` level. Set `RUST_LOG=battery=debug` to include SMC key read/write diagnostics.

Charging control probes SMC key availability at runtime instead of assuming a macOS version: it selects Golden Gate (`bfD0`/`bfE0`/`bfF0`), Tahoe (`CHTE`), or legacy (`CH0B`/`CH0C`) keys according to the keys and data widths exposed by the machine firmware. Adapter control similarly detects `CHIE`, `CH0I`, and `CH0J`. Unsupported SMC writes fail visibly; key availability can depend on firmware and hardware, even on the same macOS release.

The utility targets Apple Silicon macOS. Build with `cargo build --release` and make sure the configured SMC executable is installed and executable.
