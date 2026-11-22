//! Native manager adapters. Definitions are pure functions; commands never use a shell.

use std::{
    fmt::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use super::{Config, Paths, ServiceError, private_write, remove_if_exists, session::Session};
use crate::private_file::PrivateFile;

const UNIT: &str = "beankeeper-desktop.service";
const LABEL: &str = "ai.govcraft.beankeeper-desktop";
const WINDOWS_STOP_SIGNAL: &str = "service-stop.request";

pub(super) fn check_user() -> Result<(), ServiceError> {
    #[cfg(unix)]
    if output("id", &["-u"])? == "0" {
        return Err(ServiceError::Configuration(
            "install the service as the desktop user, without sudo; package hooks must select the logged-in owner",
        ));
    }
    Ok(())
}

pub(super) fn install(paths: &Paths, config: &Config) -> Result<(), ServiceError> {
    let log = paths.log();
    if log.try_exists()? {
        PrivateFile::open(&log)?;
    } else {
        private_write(&log, b"")?;
    }
    if cfg!(target_os = "linux") {
        private_write(&linux_artifact(paths), systemd(paths, config)?.as_bytes())?;
    } else if cfg!(target_os = "macos") {
        private_write(&paths.definition, launch_agent(paths, config)?.as_bytes())?;
    } else if cfg!(windows) {
        private_write(&paths.logs.join("service-stdout.log"), b"")?;
        private_write(
            &paths.state.join("service-run.ps1"),
            windows_runner(paths, config)?.as_bytes(),
        )?;
        private_write(&paths.definition, windows_task(paths)?.as_bytes())?;
        powershell(paths, "install")?;
    } else {
        return Err(ServiceError::Configuration(
            "background services require Linux, macOS or Windows",
        ));
    }
    Ok(())
}

pub(super) fn start(paths: &Paths, session: &Session) -> Result<(), ServiceError> {
    if cfg!(target_os = "linux") {
        systemctl(session, &["unmask", UNIT])?;
        systemctl(session, &["link", "--force", text(&linux_artifact(paths))?])?;
        systemctl(session, &["daemon-reload"])?;
        systemctl(session, &["enable", "--now", UNIT])?;
        // A failed unit may still be inside its restart delay.
        systemctl(session, &["start", UNIT])
    } else if cfg!(target_os = "macos") {
        let domain = domain()?;
        checked("launchctl", &["enable", &format!("{domain}/{LABEL}")])?;
        if success("launchctl", &["print", &format!("{domain}/{LABEL}")])? {
            checked("launchctl", &["kickstart", &format!("{domain}/{LABEL}")])
        } else {
            checked(
                "launchctl",
                &["bootstrap", &domain, text(&paths.definition)?],
            )
        }
    } else {
        remove_if_exists(&paths.state.join(WINDOWS_STOP_SIGNAL))?;
        powershell(paths, "start")
    }
}

pub(super) fn stop(paths: &Paths, session: &Session) -> Result<(), ServiceError> {
    if cfg!(target_os = "linux") {
        // Inspect the policy before stopping. Mask replaces the owned linked unit,
        // preventing both crash restarts and activation on the next login.
        let _policy = command_output(
            systemctl_command(session),
            &["show", UNIT, "--property=Restart", "--value"],
        )?;
        let masked = std::fs::read_link(&paths.definition)
            .is_ok_and(|target| target == Path::new("/dev/null"));
        if paths.definition.try_exists()? && !masked {
            systemctl(session, &["disable", "--now", UNIT])?;
        }
        systemctl(session, &["mask", "--now", "--force", UNIT])
    } else if cfg!(target_os = "macos") {
        let domain = domain()?;
        let target = format!("{domain}/{LABEL}");
        checked("launchctl", &["disable", &target])?;
        if success("launchctl", &["print", &target])? {
            checked("launchctl", &["bootout", &target])?;
        }
        Ok(())
    } else {
        private_write(&paths.state.join(WINDOWS_STOP_SIGNAL), b"stop")?;
        powershell(paths, "stop")
    }
}

pub(super) fn uninstall(paths: &Paths, session: &Session) -> Result<(), ServiceError> {
    stop(paths, session)?;
    if cfg!(target_os = "linux") {
        systemctl(session, &["unmask", UNIT])?;
        remove_if_exists(&linux_artifact(paths))?;
        systemctl(session, &["daemon-reload"])?;
    } else if cfg!(target_os = "macos") {
        remove_if_exists(&paths.definition)?;
    } else {
        powershell(paths, "uninstall")?;
        remove_if_exists(&paths.definition)?;
        remove_if_exists(&paths.state.join("service-run.ps1"))?;
        remove_if_exists(&paths.state.join(WINDOWS_STOP_SIGNAL))?;
    }
    Ok(())
}

fn systemctl_command(session: &Session) -> Command {
    let mut command = manager_command("systemctl");
    session.apply(&mut command);
    command.arg("--user");
    command
}
fn systemctl(session: &Session, args: &[&str]) -> Result<(), ServiceError> {
    check_command(systemctl_command(session), args)
}
fn linux_artifact(paths: &Paths) -> std::path::PathBuf {
    paths.state.join("systemd").join(UNIT)
}
fn domain() -> Result<String, ServiceError> {
    Ok(format!("gui/{}", output("id", &["-u"])?))
}
fn text(path: &Path) -> Result<&str, ServiceError> {
    path.to_str()
        .ok_or(ServiceError::Configuration("service paths must be Unicode"))
}
fn manager_command(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Lifecycle calls from GUI/stdio clients must not flash console windows.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.stdin(Stdio::null());
    command
}

fn checked(program: &str, args: &[&str]) -> Result<(), ServiceError> {
    check_command(manager_command(program), args)
}
fn check_command(command: Command, args: &[&str]) -> Result<(), ServiceError> {
    if command_succeeded(command, args)? {
        Ok(())
    } else {
        Err(ServiceError::Manager(
            "OS service manager refused the operation; ensure a logged-in user session (Linux requires a systemd user bus), then retry",
        ))
    }
}
fn success(program: &str, args: &[&str]) -> Result<bool, ServiceError> {
    command_succeeded(manager_command(program), args)
}
fn command_succeeded(mut command: Command, args: &[&str]) -> Result<bool, ServiceError> {
    Ok(command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}
fn output(program: &str, args: &[&str]) -> Result<String, ServiceError> {
    command_output(manager_command(program), args)
}
fn command_output(mut command: Command, args: &[&str]) -> Result<String, ServiceError> {
    let output = command.args(args).output()?;
    if !output.status.success() {
        return Err(ServiceError::Manager(
            "cannot query the OS service manager; ensure your user session is available",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}
fn powershell(paths: &Paths, operation: &str) -> Result<(), ServiceError> {
    if powershell_success(paths, operation)? {
        Ok(())
    } else {
        Err(ServiceError::Manager(
            "Task Scheduler refused the operation; run as the logged-in desktop user and check Task Scheduler history",
        ))
    }
}
fn powershell_success(paths: &Paths, operation: &str) -> Result<bool, ServiceError> {
    success(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            text(&paths.definition)?,
            operation,
        ],
    )
}
fn systemd_word(value: &str) -> Result<String, ServiceError> {
    clean(value)?;
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}
fn clean(value: &str) -> Result<(), ServiceError> {
    if value.chars().any(char::is_control) {
        return Err(ServiceError::Configuration(
            "service paths cannot contain control characters",
        ));
    }
    Ok(())
}
fn xml(value: &str) -> Result<String, ServiceError> {
    clean(value)?;
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}
fn ps(value: &str) -> Result<String, ServiceError> {
    clean(value)?;
    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('\'');
    for ch in value.chars() {
        // PowerShell treats typographic single quotes as delimiters too.
        if matches!(ch, '\'' | '\u{2018}'..='\u{201b}') {
            literal.push(ch);
        }
        literal.push(ch);
    }
    literal.push('\'');
    Ok(literal)
}
fn environment(paths: &Paths) -> Vec<(&'static str, &Path)> {
    vec![
        (
            if cfg!(windows) { "USERPROFILE" } else { "HOME" },
            &paths.home,
        ),
        ("BEANKEEPER_SERVICE_DATA_DIR", &paths.data),
        ("BEANKEEPER_DESKTOP_STATE_DIR", &paths.state),
        ("BEANKEEPER_SERVICE_LOG_DIR", &paths.logs),
    ]
}

pub(super) fn systemd(paths: &Paths, config: &Config) -> Result<String, ServiceError> {
    // systemd restricts characters in the first executable token, even when
    // quoted. env accepts the owner's full path as an argument and execs it
    // in the same process, preserving the PID tracked by systemd.
    let executable = systemd_word(text(&config.executable)?)?;
    let mut env = String::new();
    for (key, path) in environment(paths) {
        // Environment= does not expand dollar signs; only specifiers need escaping.
        let value = format!("{key}={}", text(path)?);
        clean(&value)?;
        let _ = writeln!(
            env,
            "Environment=\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
        );
    }
    let log = systemd_word(&format!("append:{}", paths.log().display()))?.replace("$$", "$");
    Ok(format!(
        "[Unit]\nDescription=Beankeeper Desktop local agent service\nAfter=graphical-session-pre.target\nStartLimitIntervalSec=0\n\n[Service]\nType=simple\nExecStart=/usr/bin/env -- {executable} service run\n{env}Restart=on-failure\nRestartSec=2\nTimeoutStopSec=20\nUMask=0077\nStandardOutput={log}\nStandardError={log}\n\n[Install]\nWantedBy=default.target\n"
    ))
}

pub(super) fn launch_agent(paths: &Paths, config: &Config) -> Result<String, ServiceError> {
    let executable = xml(text(&config.executable)?)?;
    let log = xml(text(&paths.log())?)?;
    let mut env = String::new();
    for (key, path) in environment(paths) {
        let _ = write!(
            env,
            "<key>{key}</key><string>{}</string>",
            xml(text(path)?)?
        );
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array><string>{executable}</string><string>service</string><string>run</string></array>\n<key>EnvironmentVariables</key><dict>{env}</dict>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n<key>ThrottleInterval</key><integer>2</integer>\n<key>Umask</key><integer>63</integer>\n<key>StandardOutPath</key><string>{log}</string>\n<key>StandardErrorPath</key><string>{log}</string>\n</dict></plist>\n"
    ))
}
fn windows_runner(paths: &Paths, config: &Config) -> Result<String, ServiceError> {
    // Windows PowerShell 5.1 needs a BOM for non-ASCII owner paths.
    let mut script = "\u{feff}$ErrorActionPreference = 'Stop'\n".to_owned();
    for (key, path) in environment(paths) {
        let _ = writeln!(script, "$env:{key} = {}", ps(text(path)?)?);
    }
    let executable = ps(text(&config.executable)?)?;
    let stdout = ps(text(&paths.logs.join("service-stdout.log"))?)?;
    let stderr = ps(text(&paths.log())?)?;
    let stop = ps(text(&paths.state.join(WINDOWS_STOP_SIGNAL))?)?;
    let _ = write!(
        script,
        r"$stop = {stop}
# Keep the scheduled action alive while the host restarts. Task Scheduler
# still supplies the logon trigger and retries an unexpected launcher exit.
while (-not (Test-Path -LiteralPath $stop)) {{
    $process = Start-Process -FilePath {executable} -ArgumentList 'service','run' -NoNewWindow -PassThru -RedirectStandardOutput {stdout} -RedirectStandardError {stderr}
    try {{
        while (-not $process.WaitForExit(250)) {{
            if (Test-Path -LiteralPath $stop) {{
                try {{ $process.Kill() }} catch {{ if (-not $process.HasExited) {{ throw }} }}
                $process.WaitForExit()
                break
            }}
        }}
    }} finally {{
        $process.Dispose()
    }}
    if (-not (Test-Path -LiteralPath $stop)) {{ Start-Sleep -Milliseconds 2000 }}
}}
Set-Content -LiteralPath $stop -Value 'stopped' -Encoding ascii
exit 0
"
    );
    Ok(script)
}
fn windows_task(paths: &Paths) -> Result<String, ServiceError> {
    let runner = text(&paths.state.join("service-run.ps1"))?.to_owned();
    // Windows paths cannot contain double quotes. PowerShell's literal handles apostrophes.
    let arguments = ps(&format!(
        "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File \"{runner}\""
    ))?;
    let stop = ps(text(&paths.state.join(WINDOWS_STOP_SIGNAL))?)?;
    let script = format!(
        r#"param([ValidateSet('install','start','stop','uninstall')][string]$Operation)
$ErrorActionPreference = 'Stop'
$user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$name = "Beankeeper-Desktop-$sid"
$task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
switch ($Operation) {{
    'install' {{
        $action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument {arguments}
        $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
        $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
        $settings = New-ScheduledTaskSettingsSet -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
    }}
    'start' {{ Enable-ScheduledTask -TaskName $name | Out-Null; Start-ScheduledTask -TaskName $name }}
    'stop' {{
        if ($task) {{
            $wasRunning = $task.State -eq 'Running' -or $task.State -eq 'Queued'
            Disable-ScheduledTask -TaskName $name | Out-Null
            if ($wasRunning) {{
                $deadline = [DateTime]::UtcNow.AddSeconds(8)
                while ([DateTime]::UtcNow -lt $deadline) {{
                    if ((Get-Content -LiteralPath {stop} -Raw -ErrorAction SilentlyContinue) -match '^stopped\s*$') {{ break }}
                    Start-Sleep -Milliseconds 100
                }}
                # Also stop an older registered launcher that lacks the signal protocol.
                Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
            }}
        }}
    }}
    'uninstall' {{ if ($task) {{ Unregister-ScheduledTask -TaskName $name -Confirm:$false }} }}
}}
"#
    );
    Ok(format!("\u{feff}{script}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn std::error::Error>>;
    #[cfg(target_os = "linux")]
    #[test]
    fn generated_systemd_unit_passes_the_native_parser_when_available() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        if Command::new("systemd-analyze")
            .arg("--version")
            .output()
            .is_err()
        {
            return Ok(());
        }
        let root = tempfile::Builder::new()
            .prefix("bk service café 's-")
            .tempdir()?;
        let executable = root.path().join("bk");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n")?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        let paths = Paths {
            home: root.path().into(),
            data: root.path().join("data"),
            state: root.path().join("state"),
            logs: root.path().join("logs"),
            definition: root.path().join(UNIT),
        };
        let config = Config {
            executable,
            books: paths.data.join("books.db"),
            port: 8750,
        };
        std::fs::write(&paths.definition, systemd(&paths, &config)?)?;
        let result = Command::new("systemd-analyze")
            .args(["verify", "--man=no"])
            .arg(&paths.definition)
            .output()?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    }

    #[test]
    fn windows_scripts_preserve_unicode_owner_paths_with_utf8_bom() -> TestResult {
        let paths = Paths {
            home: "/café/owner's home".into(),
            data: "/café/data".into(),
            state: "/café/state".into(),
            logs: "/café/logs".into(),
            definition: "/café/task.ps1".into(),
        };
        let config = Config {
            executable: "/café/bk.exe".into(),
            books: "/café/books.db".into(),
            port: 8750,
        };
        for script in [windows_runner(&paths, &config)?, windows_task(&paths)?] {
            assert!(script.as_bytes().starts_with(&[0xef, 0xbb, 0xbf]));
            assert!(script.contains("café"));
        }
        assert_eq!(
            ps("owner's ‘file’ ‚folder‛")?,
            "'owner''s ‘‘file’’ ‚‚folder‛‛'"
        );
        let runner = windows_runner(&paths, &config)?;
        assert!(runner.contains("Start-Process"));
        assert!(runner.contains("-RedirectStandardError"));
        assert!(!runner.contains("*>>"));
        assert!(runner.contains("while (-not (Test-Path -LiteralPath $stop))"));
        assert!(runner.contains("Start-Sleep -Milliseconds 2000"));
        assert!(runner.contains("$process.Kill()"));
        let task = windows_task(&paths)?;
        assert!(task.contains("-WindowStyle Hidden"));
        assert!(task.contains("Disable-ScheduledTask"));
        assert!(task.contains("service-stop.request"));
        Ok(())
    }

    #[test]
    fn manager_definitions_escape_paths_and_enforce_user_restart_policy() -> TestResult {
        let paths = Paths {
            home: "/owner".into(),
            data: "/owner/data".into(),
            state: "/owner/state".into(),
            logs: "/owner/logs".into(),
            definition: "/owner/unit".into(),
        };
        let config = Config {
            executable: "/owner/A & B/%test/$money/it's bk".into(),
            books: "/owner/books.db".into(),
            port: 8750,
        };
        let unit = systemd(&paths, &config)?;
        assert!(unit.contains("%%test/$$money/it's bk\" service run"));
        assert!(unit.contains("Restart=on-failure\n"));
        assert!(unit.contains("UMask=0077\n"));
        assert!(!unit.contains("User="));
        let plist = launch_agent(&paths, &config)?;
        assert!(plist.contains("A &amp; B"));
        assert!(plist.contains("it&apos;s bk"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
        let task = windows_task(&paths)?;
        assert!(task.contains("-LogonType Interactive -RunLevel Limited"));
        assert!(task.contains("-RestartCount 999"));
        assert!(task.contains("Disable-ScheduledTask"));
        assert_eq!(ps("a'b")?, "'a''b'");
        assert!(systemd_word("bad\npath").is_err());
        assert!(xml("bad\rpath").is_err());
        Ok(())
    }
}
