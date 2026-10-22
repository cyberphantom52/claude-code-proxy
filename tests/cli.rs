use assert_cmd::Command;
use predicates::{prelude::PredicateBooleanExt, str::contains};
#[cfg(unix)]
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn version_aliases_print_expected_version() -> Result<(), Box<dyn std::error::Error>> {
    let expected = format!("claude-code-proxy {}", env!("CARGO_PKG_VERSION"));

    for arg in ["--version", "-v", "version"] {
        let mut cmd = Command::cargo_bin("claude-code-proxy")?;
        cmd.env_clear()
            .arg(arg)
            .assert()
            .success()
            .stdout(contains(expected.clone()));
    }
    Ok(())
}

#[test]
fn models_prints_only_codex_models() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let output = Command::cargo_bin("claude-code-proxy")?
        .env_clear()
        .arg("models")
        .env("CCP_CONFIG_DIR", temp.path())
        .env("HOME", temp.path())
        .env("XDG_STATE_HOME", temp.path())
        .env("CCP_CODEX_MODEL_DISCOVERY", "0")
        .output()?;
    assert!(output.status.success());
    let out = String::from_utf8(output.stdout)?;
    assert!(out.starts_with("codex: "));
    assert_eq!(out.lines().count(), 1);
    for removed in ["kimi", "grok", "cursor", "opencode", "composer-2.5"] {
        assert!(!out.contains(removed), "{out}");
    }
    assert!(out.contains("gpt-6-sol"));
    assert!(out.contains("haiku"));
    Command::cargo_bin("claude-code-proxy")?
        .env_clear()
        .args(["models", "--full"])
        .assert()
        .code(2);
    Ok(())
}

#[test]
fn help_describes_visible_commands_and_hides_demo() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.env_clear().arg("--help");
    let output = cmd.output()?;
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    for description in [
        "Print version information",
        "Start the proxy server and monitor",
        "List supported Codex models",
        "Manage Codex authentication",
    ] {
        assert!(stdout.contains(description), "missing: {description}");
    }
    assert!(!stdout.contains("demo"));
    assert!(!stdout.contains("mock data and no proxy server"));
    Ok(())
}

#[test]
fn invalid_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("claude-code-proxy")?
        .env_clear()
        .arg("definitely-not-a-command")
        .assert()
        .failure()
        .code(2);
    Ok(())
}

#[test]
fn removed_backend_commands_are_rejected_without_deleting_auth()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    for backend in ["kimi", "grok", "cursor", "opencode"] {
        let auth_path = temp.path().join(backend).join("auth.json");
        std::fs::create_dir_all(auth_path.parent().unwrap())?;
        std::fs::write(&auth_path, "existing credentials")?;
        for action in ["login", "device", "status", "logout"] {
            Command::cargo_bin("claude-code-proxy")?
                .env_clear()
                .args([backend, "auth", action])
                .env("CCP_CONFIG_DIR", temp.path())
                .assert()
                .code(2)
                .stderr(contains("unrecognized subcommand"));
            assert_eq!(std::fs::read_to_string(&auth_path)?, "existing credentials");
        }
    }
    Command::cargo_bin("claude-code-proxy")?
        .env_clear()
        .args(["codex", "auth", "logout"])
        .env("CCP_CONFIG_DIR", temp.path())
        .env("HOME", temp.path())
        .assert()
        .success();
    for backend in ["kimi", "grok", "cursor", "opencode"] {
        assert_eq!(
            std::fs::read_to_string(temp.path().join(backend).join("auth.json"))?,
            "existing credentials"
        );
    }
    Ok(())
}

#[test]
fn stale_alias_selection_fails_with_migration_error() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    Command::cargo_bin("claude-code-proxy")?
        .env_clear()
        .arg("models")
        .env("CCP_CONFIG_DIR", temp.path())
        .env("CCP_ALIAS_PROVIDER", "kimi")
        .assert()
        .failure()
        .stderr(contains("CCP_ALIAS_PROVIDER").and(contains("ChatGPT subscription")));
    std::fs::write(
        temp.path().join("config.json"),
        r#"{"aliasProvider":"kimi"}"#,
    )?;
    Command::cargo_bin("claude-code-proxy")?
        .env_clear()
        .args(["serve", "--no-monitor", "--port", "0"])
        .env("CCP_CONFIG_DIR", temp.path())
        .assert()
        .failure()
        .stderr(contains("aliasProvider").and(contains("ChatGPT subscription")));
    Ok(())
}

#[test]
fn codex_browser_and_device_auth_commands_remain_available()
-> Result<(), Box<dyn std::error::Error>> {
    for action in ["login", "device"] {
        Command::cargo_bin("claude-code-proxy")?
            .env_clear()
            .args(["codex", "auth", action, "--help"])
            .assert()
            .success();
    }
    Ok(())
}

#[test]
fn removed_launch_command_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("claude-code-proxy")?
        .env_clear()
        .args(["launch", "--print-env"])
        .assert()
        .code(2)
        .stderr(contains("unrecognized subcommand"));
    Ok(())
}

#[cfg(unix)]
struct ChildGuard(Child);

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn wait_for_service(
    child: &mut ChildGuard,
    port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                if let Some(status) = child.0.try_wait()? {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.0.stderr.take() {
                        pipe.read_to_string(&mut stderr)?;
                    }
                    return Err(format!("service exited with {status}: {stderr}").into());
                }
                let _ = error;
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(unix)]
fn send_signal(child: &ChildGuard, signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("kill")
        .args([signal, &child.0.id().to_string()])
        .status()?;
    if !status.success() {
        return Err(format!("kill {signal} failed with {status}").into());
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_exit(
    child: &mut ChildGuard,
    timeout: Duration,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("plain service did not exit after the second signal".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn plain_service_exits_on_second_signal(signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let upstream = TcpListener::bind("127.0.0.1:0")?;
    let upstream_url = format!("http://{}", upstream.local_addr()?);
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let fixture = thread::spawn(move || {
        let (stream, _) = upstream.accept().unwrap();
        accepted_tx.send(()).unwrap();
        let _ = release_rx.recv();
        drop(stream);
    });

    let config = TempDir::new()?;
    let auth_dir = config.path().join("codex");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        auth_dir.join("auth.json"),
        r#"{"access":"test","refresh":"test","expires":4102444800000,"accountId":"test"}"#,
    )?;
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"))
        .env_clear()
        .args(["serve", "--no-monitor", "--port", &port.to_string()])
        .env("CCP_CONFIG_DIR", config.path())
        .env("CCP_CODEX_BASE_URL", upstream_url)
        .env("CCP_CODEX_TRANSPORT", "http")
        .env("CCP_CODEX_MODEL_DISCOVERY", "0")
        .env("HOME", config.path())
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard(child);
    let mut downstream = wait_for_service(&mut child, port)?;
    let body =
        br#"{"model":"gpt-5.4","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#;
    write!(
        downstream,
        "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )?;
    downstream.write_all(body)?;
    accepted_rx.recv_timeout(Duration::from_secs(20))?;

    send_signal(&child, signal)?;
    thread::sleep(Duration::from_millis(200));
    assert!(child.0.try_wait()?.is_none());
    send_signal(&child, signal)?;
    let status = wait_for_exit(&mut child, Duration::from_secs(2));
    let _ = release_tx.send(());
    fixture.join().unwrap();

    assert_eq!(status?.code(), Some(130));
    Ok(())
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_ctrl_c() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-INT")
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_sigterm() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-TERM")
}
