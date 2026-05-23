//! Windows Startup-folder install. Writes a `.lnk` to
//! `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\alleycat.lnk`
//! using the `mslnk` crate (pure-Rust, no COM, no admin).
//!
//! When a `<binary>-startup.exe` sidecar is installed next to the CLI binary,
//! the shortcut points at that GUI-subsystem launcher. The launcher starts the
//! real daemon with `serve` and no console window, while the main binary stays
//! a normal console CLI for interactive commands.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use mslnk::{ShellLink, ShowCommand};

use crate::paths;
use crate::service::DAEMON_SUBCOMMAND;

pub(super) fn install() -> anyhow::Result<()> {
    let lnk_path = paths::windows_startup_lnk_path()?;
    let exe = std::env::current_exe().context("resolving current executable")?;
    write_startup_lnk(&lnk_path, &exe)
}

pub(super) fn uninstall() -> anyhow::Result<()> {
    let lnk_path = paths::windows_startup_lnk_path()?;
    if lnk_path.exists() {
        std::fs::remove_file(&lnk_path)
            .with_context(|| format!("removing {}", lnk_path.display()))?;
    }
    Ok(())
}

pub(super) fn write_startup_lnk(lnk_path: &Path, exe: &Path) -> anyhow::Result<()> {
    if let Some(parent) = lnk_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let target = startup_link_target(exe);
    let target_str = target.target.to_str().ok_or_else(|| {
        anyhow!(
            "executable path is not valid UTF-8: {}",
            target.target.display()
        )
    })?;
    let mut lnk = ShellLink::new(target_str)
        .with_context(|| format!("creating ShellLink for {target_str}"))?;
    lnk.header_mut()
        .set_show_command(ShowCommand::ShowMinNoActive);
    lnk.set_arguments(target.arguments);
    lnk.create_lnk(lnk_path)
        .with_context(|| format!("writing {}", lnk_path.display()))?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct StartupLinkTarget {
    target: PathBuf,
    arguments: Option<String>,
}

fn startup_link_target(exe: &Path) -> StartupLinkTarget {
    if let Some(launcher) = startup_launcher_path(exe).filter(|launcher| launcher.exists()) {
        return StartupLinkTarget {
            target: launcher,
            arguments: None,
        };
    }

    StartupLinkTarget {
        target: exe.to_owned(),
        arguments: Some(DAEMON_SUBCOMMAND.to_string()),
    }
}

fn startup_launcher_path(exe: &Path) -> Option<PathBuf> {
    let stem = exe.file_stem()?.to_str()?;
    Some(exe.with_file_name(format!("{stem}-startup.exe")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let mut path = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        path.push(format!(
            "alleycat-svc-windows-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn write_startup_lnk_creates_file() {
        let tmp = tempdir();
        let lnk = tmp.join("alleycat.lnk");
        let exe = std::env::current_exe().expect("current_exe");
        write_startup_lnk(&lnk, &exe).expect("write_startup_lnk");
        assert!(lnk.exists());
        let bytes = std::fs::read(&lnk).expect("read lnk");
        assert!(!bytes.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn startup_link_prefers_windowless_launcher_when_present() {
        let tmp = tempdir();
        let exe = tmp.join("alleycat.exe");
        let launcher = tmp.join("alleycat-startup.exe");
        std::fs::write(&exe, b"exe").expect("write exe");
        std::fs::write(&launcher, b"launcher").expect("write launcher");

        assert_eq!(
            startup_link_target(&exe),
            StartupLinkTarget {
                target: launcher,
                arguments: None,
            }
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn startup_link_falls_back_to_cli_when_launcher_is_absent() {
        let tmp = tempdir();
        let exe = tmp.join("alleycat.exe");
        std::fs::write(&exe, b"exe").expect("write exe");

        assert_eq!(
            startup_link_target(&exe),
            StartupLinkTarget {
                target: exe,
                arguments: Some(DAEMON_SUBCOMMAND.to_string()),
            }
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
