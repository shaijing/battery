# battery

Rust CLI for battery controls on Apple Silicon macOS. It uses `pmset -g batt` for battery readings and calls AppleSMC through IOKit directly for SMC access.

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

`charging on|off` controls whether the battery may charge. `adapter on|off` controls adapter input power: `on` enables adapter input, while `off` disables it and makes the battery supply the computer. These controls are independent. Successful manual settings are saved in `~/.battery/config.toml` under `[controls]`; automatic changes made by charge, discharge, or maintenance do not overwrite them.

Run `battery visudo` once to install and validate a sudoers file for the current user. It installs a root-owned helper and allows only supported fixed SMC writes plus the validated Golden Gate wrapper, so the LaunchAgent can manage charging limits without a password prompt. `maintain` requires firmware charge limit keys (`bfD0`/`bfE0`/`bfF0`), saves its target in `~/.battery/config.toml`, and registers a per-user LaunchAgent at `~/Library/LaunchAgents/battery.plist` to restore the limit at login. It does not write charging-enable or adapter-input keys. Firmware without native charge limit keys reports an error instead of changing charging or adapter state to simulate a limit.

`maintain` checks that the LaunchAgent is running before reporting success. `status` reports a saved target as active only while the agent is running. Manual `charging`, `charge`, and `discharge` commands stop maintenance first; `adapter` commands leave maintenance running. The maintenance agent only sets the firmware charge limit and leaves charging-enable and adapter-input settings alone. `status` reports the macOS battery charging state rather than treating the firmware limit flag as a charging state.

`status_csv` is also available for the original menu bar app integration and prints percentage, remaining time, charging state, discharging state, and maintenance target as comma-separated fields.

Logs use `tracing`, go to stderr and `~/.battery/battery.log`, and default to the `info` level. Set `RUST_LOG=battery=debug` to include SMC key read/write diagnostics.

Charging control probes SMC key availability at runtime instead of assuming a macOS version: it selects Golden Gate (`bfD0`/`bfE0`/`bfF0`), Tahoe (`CHTE`), or legacy (`CH0B`/`CH0C`) keys according to the keys and data widths exposed by the machine firmware. Adapter control similarly detects `CHIE`, `CH0I`, and `CH0J`. Unsupported SMC writes fail visibly; key availability can depend on firmware and hardware, even on the same macOS release.

The utility targets Apple Silicon macOS. Build with `cargo build --release`; SMC access is built into the binary and links against IOKit.
