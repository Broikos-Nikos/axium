use anyhow::Result;
use tokio::process::Command;

/// Check if a command invokes sudo as a standalone command word.
/// Only matches `sudo` at the start of a command or after shell metacharacters,
/// ignoring occurrences inside quoted strings (best-effort).
fn cmd_uses_sudo(cmd: &str) -> bool {
    // Split on shell metacharacters and check for standalone "sudo" tokens
    cmd.split(|c: char| c.is_whitespace() || c == ';' || c == '|' || c == '&' || c == '(' || c == '`')
        .any(|word| word == "sudo")
}

/// Set the child process to run in its own process group (for clean tree kill).
#[cfg(unix)]
fn set_process_group(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}
#[cfg(not(unix))]
fn set_process_group(_cmd: &mut Command) {}

/// Kill the entire process group of a child.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL); }
}
#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

pub async fn run_command(cmd: &str, timeout_secs: u64, sudo_password: Option<&str>, working_dir: Option<&str>) -> Result<(String, String, i32)> {
    let needs_sudo = sudo_password.is_some() && cmd_uses_sudo(cmd);

    let actual_cmd = if needs_sudo {
        let stripped = cmd.strip_prefix("sudo ").unwrap_or(cmd);
        format!("sudo -S {}", stripped)
    } else {
        cmd.to_string()
    };

    let mut builder = Command::new("bash");
    builder.arg("-c").arg(&actual_cmd).kill_on_drop(true);
    set_process_group(&mut builder);
    if let Some(dir) = working_dir {
        if !dir.is_empty() {
            builder.current_dir(dir);
        }
    }
    builder
        .stdin(if needs_sudo {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = builder.spawn()?;
    let child_pid = child.id().unwrap_or(0);

    if needs_sudo {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let pw = sudo_password.unwrap();
            if let Err(e) = stdin.write_all(format!("{}\n", pw).as_bytes()).await {
                tracing::warn!("Failed to write sudo password to stdin: {}", e);
            }
            drop(stdin);
        }
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| {
        // Kill entire process group on timeout
        if child_pid > 0 { kill_process_group(child_pid); }
        anyhow::anyhow!("Command timed out after {}s", timeout_secs)
    })??;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    Ok((stdout, stderr, code))
}

/// Maximum lifetime for background processes (30 minutes).
const BG_MAX_LIFETIME_SECS: u64 = 30 * 60;

/// Spawn a background process, return its PID.
/// The process is automatically killed after BG_MAX_LIFETIME_SECS.
pub async fn spawn_background(cmd: &str, working_dir: Option<&str>) -> Result<u32> {
    let mut builder = Command::new("bash");
    builder.arg("-c").arg(cmd);
    set_process_group(&mut builder);
    if let Some(dir) = working_dir {
        if !dir.is_empty() {
            builder.current_dir(dir);
        }
    }
    builder
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    let mut child = builder.spawn()?;

    let pid = child.id()
        .ok_or_else(|| anyhow::anyhow!("Failed to obtain child PID — process may have exited immediately"))?;

    // Spawn a watchdog that kills the process group after the max lifetime
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(BG_MAX_LIFETIME_SECS)) => {
                kill_process_group(pid);
                let _ = child.kill().await;
                tracing::warn!(pid, "Background process killed after {}s lifetime limit", BG_MAX_LIFETIME_SECS);
            }
            _ = child.wait() => {
                // Process exited naturally, nothing to do
            }
        }
    });

    Ok(pid)
}
