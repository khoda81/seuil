//! Interactive first-time setup.
//!
//! `seuil setup [--reauth]` enrolls the Samsung account when needed, lists the
//! BlueZ paired devices for the user to pick from, writes the config, and
//! installs and starts the systemd user unit.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use crate::auth::Credentials;
use crate::doctor::command_available;
use crate::{config, Cli};

/// Systemd unit name, also the service name.
const SERVICE: &str = "seuil.service";

/// Path systemd reads the BlueZ drop-in from.
pub const BLUEZ_DROPIN_PATH: &str = "/etc/systemd/system/bluetooth.service.d/override.conf";

/// Standard bluetoothd locations, tried in order when the drop-in is built.
/// SteamOS and Arch ship `/usr/lib/bluetooth/bluetoothd`; the Debian family and
/// Fedora ship `/usr/libexec/bluetooth/bluetoothd`. [FR-005]
const BLUEZD_CANDIDATES: [&str; 2] = [
    "/usr/lib/bluetooth/bluetoothd",
    "/usr/libexec/bluetooth/bluetoothd",
];

/// Resolves the bluetoothd executable this system ships, so the printed drop-in
/// never carries an `ExecStart` path from another distribution.
fn resolve_bluetoothd() -> Option<&'static str> {
    BLUEZD_CANDIDATES
        .iter()
        .copied()
        .find(|candidate| executable_file(Path::new(candidate)))
}

/// Builds the optional drop-in that starts the resolved bluetoothd with the
/// experimental interfaces. The current code path does not need it, so it is
/// printed and only installed behind `--install-bluez-dropin`.
fn bluez_dropin(bluetoothd: &str) -> String {
    format!(
        "# The ExecStart path below was detected on this system at setup time; edit it\n\
         # if your distribution installs bluetoothd elsewhere.\n\
         [Service]\n\
         ExecStart=\n\
         ExecStart={bluetoothd} -E\n"
    )
}

/// Reports whether `path` is a regular file with an execute bit set.
fn executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Which role a paired device most likely plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Phone,
    Buds,
    Other,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::Phone => "phone",
            Role::Buds => "buds",
            Role::Other => "device",
        }
    }
}

/// One paired BlueZ device shown in the picker.
#[derive(Debug, Clone)]
struct PairedDevice {
    address: String,
    name: String,
    role: Role,
}

/// Classifies a device by Bluetooth major device class and its name.
fn classify(class: Option<u32>, name: &str) -> Role {
    let major = class.map(|value| (value >> 8) & 0x1F).unwrap_or(0);
    let lower = name.to_ascii_lowercase();
    if lower.contains("buds") {
        return Role::Buds;
    }
    if major == 0x04 {
        return Role::Buds;
    }
    if major == 0x02 || lower.contains("galaxy") || lower.contains("samsung") {
        return Role::Phone;
    }
    Role::Other
}

/// Runs the setup wizard.
pub async fn run(cli: &Cli, reauth: bool, install_bluez_dropin: bool) -> Result<()> {
    let executable = std::env::current_exe().context("resolve the running Seuil executable")?;
    let unit = unit_text(&executable)?;
    println!("Seuil setup");
    if !command_available("bluetoothctl") {
        println!(
            "warning: `bluetoothctl` is not installed; pairing the phone and the Buds will fail"
        );
    }
    println!(
        "On a Steam Deck, run setup in Desktop Mode: sign-in opens a browser and the daemon \
         needs your user session."
    );

    ensure_enrolled(cli, reauth).await?;

    let devices = paired_devices(&cli.adapter).await?;
    if devices.is_empty() {
        anyhow::bail!(
            "no paired Bluetooth devices found; pair the phone and the Buds in \
             bluetoothctl first, then re-run `seuil setup`"
        );
    }

    println!("\nPaired Bluetooth devices:");
    for (index, device) in devices.iter().enumerate() {
        println!(
            "{:>3}. {:<32} {}  [{}]",
            index + 1,
            device.name,
            device.address,
            device.role.label()
        );
    }

    let phone_default = devices.iter().position(|d| d.role == Role::Phone);
    let buds_default = devices.iter().position(|d| d.role == Role::Buds);
    println!("\nEnter a list number, or press Enter to accept the default.");
    let phone_index = prompt_index(
        "Select the phone by list number",
        phone_default,
        devices.len(),
    )
    .await?;
    let buds_index = prompt_index(
        "Select the Buds by list number",
        buds_default,
        devices.len(),
    )
    .await?;
    if phone_index == buds_index {
        anyhow::bail!("the phone and the Buds must be different devices");
    }
    let phone = devices[phone_index].address.clone();
    let buds = devices[buds_index].address.clone();

    let dir = config::config_dir();
    ensure_private_dir(&dir)?;
    let config_path = dir.join("config.toml");
    let text = format!(
        "# Seuil configuration\nphone_mac = \"{phone}\"\nbuds_mac = \"{buds}\"\n# adapter = \"{}\"\n# discover = true\n",
        cli.adapter
    );
    write_atomic(&config_path, text.as_bytes(), 0o600)?;
    println!("\nWrote {}", config_path.display());

    report_optional_tools();
    install_service(&unit)?;
    println!("Installed and started {SERVICE}");
    print_bluez_dropin(install_bluez_dropin)?;
    println!("Check the connection with: journalctl --user -u {SERVICE} -f");
    println!("Verify the install with: seuil doctor");
    Ok(())
}

/// Enrolls the Samsung account when no credentials exist or `--reauth` is set.
async fn ensure_enrolled(cli: &Cli, reauth: bool) -> Result<()> {
    let credentials = Credentials::load()?;
    let enrolled = credentials
        .as_ref()
        .is_some_and(|stored| !stored.account_key.is_empty());
    if enrolled && !reauth {
        println!("Samsung account already enrolled");
        return Ok(());
    }

    println!("Samsung account enrollment (a browser window opens for sign-in)");
    let mut auth_args: Vec<String> = vec![
        "seuil".into(),
        "--auth-login".into(),
        "--auth-server".into(),
        cli.auth_server.clone(),
        "--adapter".into(),
        cli.adapter.clone(),
    ];
    if let Some(address) = &cli.bt_address {
        auth_args.push("--bt-address".into());
        auth_args.push(address.clone());
    }
    let auth_cli = Cli::parse_from(auth_args);
    crate::run_auth(&auth_cli).await?;
    info!("Samsung account enrollment complete");
    Ok(())
}

/// Lists paired BlueZ devices with a likely role.
async fn paired_devices(adapter_name: &str) -> Result<Vec<PairedDevice>> {
    let session = bluer::Session::new().await?;
    let adapter = session.adapter(adapter_name)?;
    adapter.set_powered(true).await?;

    let mut devices = Vec::new();
    for address in adapter.device_addresses().await? {
        let Ok(device) = adapter.device(address) else {
            continue;
        };
        if !device.is_paired().await.unwrap_or(false) {
            continue;
        }
        let alias = device.alias().await.unwrap_or_default();
        let name = device.name().await.ok().flatten().unwrap_or_default();
        let label = if !alias.is_empty() { alias } else { name };
        let class = device.class().await.unwrap_or(None);
        devices.push(PairedDevice {
            address: address.to_string(),
            role: classify(class, &label),
            name: label,
        });
    }
    devices.sort_by(|a, b| {
        let rank = |role: Role| match role {
            Role::Phone => 0,
            Role::Buds => 1,
            Role::Other => 2,
        };
        rank(a.role)
            .cmp(&rank(b.role))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(devices)
}

/// Reads a 1-based list number, defaulting to `suggested` on an empty line.
async fn prompt_index(label: &str, suggested: Option<usize>, len: usize) -> Result<usize> {
    loop {
        match suggested {
            Some(index) => print!("{label} (1-{len}) [default {}]: ", index + 1),
            None => print!("{label} (1-{len}): "),
        }
        std::io::stdout().flush()?;
        let Some(line) = crate::auth::read_stdin_line().await else {
            anyhow::bail!("no input available; pass the MACs with flags or config.toml");
        };
        let line = line.trim();
        if line.is_empty() {
            if let Some(index) = suggested {
                return Ok(index);
            }
            continue;
        }
        match line.parse::<usize>() {
            Ok(value) if (1..=len).contains(&value) => return Ok(value - 1),
            _ => println!("Enter a number between 1 and {len}."),
        }
    }
}

/// Writes the systemd user unit and enables it.
fn install_service(unit: &str) -> Result<()> {
    let unit_path = unit_path()?;
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    write_atomic(&unit_path, unit.as_bytes(), 0o644)?;

    run_systemctl(&["--user", "daemon-reload"])?;
    run_systemctl(&["--user", "enable", SERVICE])?;
    // `enable --now` leaves an already-running unit untouched, so a re-run of
    // setup would keep the daemon on the previous phone/Buds addresses.
    run_systemctl(&["--user", "restart", SERVICE])?;
    Ok(())
}

/// Path of the user unit.
pub fn unit_path() -> Result<PathBuf> {
    Ok(config::config_dir()
        .parent()
        .map(|base| base.join("systemd/user").join(SERVICE))
        .unwrap_or_else(|| PathBuf::from(SERVICE)))
}

/// Contents of the user unit.
///
/// [FR-003] User-scope ordering only: a user unit cannot depend on system units
/// such as `bluetooth.target`, and `PartOf=graphical-session.target` would stop
/// the daemon on a Gaming Mode to Desktop Mode switch.
/// `WantedBy=default.target` starts it on a pure Gaming Mode boot.
fn unit_text(executable: &Path) -> Result<String> {
    let path = executable
        .to_str()
        .with_context(|| format!("Seuil executable path is not valid UTF-8: {executable:?}"))?;
    // systemd rejects these characters in the executable even after unquoting.
    anyhow::ensure!(
        !path.bytes().any(|byte| {
            byte.is_ascii_control() || matches!(byte, b'"' | b'\'' | b'\\' | b'*' | b'?' | b'[')
        }),
        "systemd cannot use this Seuil executable path: {executable:?}"
    );
    let path = path.replace('%', "%%");
    // Quote the executable as one word and disable $ expansion with `:`.
    Ok(format!(
        "[Unit]\n\
         Description=Seuil Samsung Continuity daemon (Galaxy Buds Auto Switch)\n\
         After=pipewire.service\n\
         Wants=pipewire.service\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=:\"{path}\"\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         Environment=RUST_LOG=info\n\
         ExecStartPre=-/usr/bin/bluetoothctl show\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ))
}

/// Prints the optional BlueZ drop-in and installs it only when asked. [FR-005]
fn print_bluez_dropin(install: bool) -> Result<()> {
    println!(
        "\nOptional BlueZ experimental interfaces (advisory: the current code path does not \
         need them)"
    );
    let Some(bluetoothd) = resolve_bluetoothd() else {
        let candidates = BLUEZD_CANDIDATES.join(", ");
        println!(
            "No bluetoothd executable was found at {candidates}. A drop-in whose ExecStart \
             names a nonexistent binary would stop bluetoothd, so none was written; locate \
             your distribution's bluetoothd before installing the drop-in."
        );
        if install {
            anyhow::bail!(
                "--install-bluez-dropin was passed but no bluetoothd executable was found at \
                 {candidates}; install the drop-in manually with the correct ExecStart path"
            );
        }
        return Ok(());
    };
    let dropin = bluez_dropin(bluetoothd);
    let path = config::config_dir().join("bluetooth-override.conf");
    write_atomic(&path, dropin.as_bytes(), 0o644)
        .with_context(|| format!("write {}", path.display()))?;

    for line in dropin.lines() {
        println!("  {line}");
    }
    println!("\nInstall the drop-in with:");
    println!("  sudo install -Dm644 {} {BLUEZ_DROPIN_PATH}", path.display());
    println!("  sudo systemctl daemon-reload");
    println!("  sudo systemctl restart bluetooth");
    println!(
        "\nSteamOS 3.6 and newer migrate /etc by allowlist only. Add an entry such as\n  \
         systemd/system/bluetooth.service.d/override.conf\nto /etc/atomic-update.conf.d/ (for \
         example /etc/atomic-update.conf.d/seuil-bluez.conf), or the override is dropped on the \
         next OS update."
    );

    if !install {
        println!(
            "Nothing was written under /etc. Re-run `seuil setup --install-bluez-dropin` to apply it."
        );
        return Ok(());
    }

    let mut command = Command::new("sudo");
    command.args(["install", "-Dm644"]).arg(&path).arg(BLUEZ_DROPIN_PATH);
    let status = command.status().context("run sudo install")?;
    if !status.success() {
        anyhow::bail!(
            "`sudo install -Dm644 {} {BLUEZ_DROPIN_PATH}` failed with {status}; nothing was \
             written under /etc",
            path.display()
        );
    }

    for args in [
        &["systemctl", "daemon-reload"][..],
        &["systemctl", "restart", "bluetooth"][..],
    ] {
        let status = Command::new("sudo")
            .args(args)
            .status()
            .with_context(|| format!("run sudo {}", args.join(" ")))?;
        if !status.success() {
            anyhow::bail!(
                "`sudo {}` failed with {status}; the drop-in was written but not activated; run \
                 `seuil doctor` to check",
                args.join(" ")
            );
        }
    }
    println!("Installed {BLUEZ_DROPIN_PATH} and restarted bluetooth");
    Ok(())
}

/// Warns about optional tools that only degrade a feature. [FR-004]
fn report_optional_tools() {
    for (tool, feature) in [
        ("sdptool", "the phone SDP check is skipped (normal on SteamOS)"),
        ("pactl", "PipeWire stream activity detection is degraded"),
        ("playerctl", "MPRIS playback detection is degraded"),
    ] {
        if !command_available(tool) {
            println!("warning: `{tool}` is not installed; {feature}");
        }
    }
}

/// Creates `dir` if needed and tightens it to owner-only. [FR-022]
fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set permissions on {}", dir.display()))?;
    Ok(())
}

/// Writes `contents` to `path` through a same-directory temporary file and a
/// rename, so a crash leaves the previous content intact. [FR-022]
fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{} has no file name", path.display()))?;
    let temp = parent.join(format!(".{name}.tmp.{}", std::process::id()));

    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        // Re-apply the mode: a stale temporary file from a crashed run may
        // already exist with wider permissions.
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("set permissions on {}", temp.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temp.display()))?;
        std::fs::rename(&temp, path)
            .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
        // Persist the rename itself. A failure here only weakens crash
        // durability, so it does not fail setup.
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .context("run systemctl")?;
    if !status.success() {
        anyhow::bail!("systemctl {} exited with {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_buds_by_name() {
        assert_eq!(classify(None, "Buds Core"), Role::Buds);
        assert_eq!(classify(Some(0x240404), "Headset"), Role::Buds);
    }

    #[test]
    fn classifies_phones_by_major_class() {
        assert_eq!(classify(Some(0x005A020C), "Galaxy A57"), Role::Phone);
        assert_eq!(classify(None, "Galaxy S24"), Role::Phone);
    }

    #[test]
    fn classifies_unknown_devices_as_other() {
        assert_eq!(classify(Some(0x001F0100), "Keyboard"), Role::Other);
    }

    #[test]
    fn unit_starts_the_executable_used_for_setup() {
        for path in ["/usr/bin/seuil", "/home/desktop/.local/bin/seuil"] {
            let unit = unit_text(Path::new(path)).unwrap();
            let exec_start = format!("ExecStart=:\"{path}\"");
            assert!(unit.lines().any(|line| line == exec_start));
            assert!(unit.contains("After=pipewire.service"));
            assert!(!unit.contains("bluetooth.target"));
            assert!(!unit.contains("graphical-session.target"));
        }
    }

    #[test]
    fn unit_quotes_spaces_and_leaves_specifiers_and_dollars_literal() {
        let unit = unit_text(Path::new("/home/Desktop User/100%/${BIN}/seuil")).unwrap();
        let exec_start = "ExecStart=:\"/home/Desktop User/100%%/${BIN}/seuil\"";
        assert!(unit.lines().any(|line| line == exec_start));
    }

    #[test]
    fn unit_rejects_a_path_systemd_cannot_execute() {
        assert!(unit_text(Path::new("/home/Desktop\nUser/seuil")).is_err());
    }
}
