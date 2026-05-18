use std::process::Command;

#[test]
fn serve_help_documents_pwa_http_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_alleycat"))
        .arg("serve")
        .arg("--help")
        .output()
        .expect("run alleycat serve --help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8 help");
    assert!(stdout.contains("--serve-pwa"), "{stdout}");
    assert!(stdout.contains("--listen"), "{stdout}");
    assert!(stdout.contains("--pwa-dir"), "{stdout}");
    assert!(stdout.contains("--ws-only"), "{stdout}");
    assert!(stdout.contains("--auto-pair-tailnet"), "{stdout}");
}

#[test]
fn serve_rejects_invalid_listen_addr() {
    let output = Command::new(env!("CARGO_BIN_EXE_alleycat"))
        .arg("serve")
        .arg("--serve-pwa")
        .arg("--listen")
        .arg("not-a-socket")
        .output()
        .expect("run alleycat serve with invalid listen addr");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("--listen <LISTEN>"), "{stderr}");
}
