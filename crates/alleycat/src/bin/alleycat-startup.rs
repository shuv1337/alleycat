#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    if let Err(error) = run() {
        #[cfg(target_os = "windows")]
        let _ = error;
        #[cfg(not(target_os = "windows"))]
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "windows")]
fn run() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    let launcher = std::env::current_exe()?;
    let launcher_name = launcher
        .file_stem()
        .ok_or_else(|| invalid_input("startup launcher path has no file name"))?
        .to_string_lossy();
    let cli_name = launcher_name
        .strip_suffix("-startup")
        .ok_or_else(|| invalid_input("startup launcher name must end in -startup"))?;
    let cli = launcher.with_file_name(format!("{cli_name}.exe"));

    let mut command = std::process::Command::new(cli);
    command
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);

    command.spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn invalid_input(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

#[cfg(not(target_os = "windows"))]
fn run() -> Result<(), &'static str> {
    Err("alleycat-startup is only used by Windows autostart")
}
