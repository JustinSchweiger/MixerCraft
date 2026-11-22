//! Windows: the helper that repairs virtual devices after Fast Startup.
//!
//! "Shut down" with Fast Startup (the Windows default) signs the user out
//! and hibernates Windows itself. usbip-win2 (ADR 0007) comes back from that
//! with its port table empty, while Windows still has the virtual devices of
//! the previous session plugged into its virtual hub: every application gets
//! "device invalidated", and anything MixerCraft connects lands on a port
//! Windows considers occupied and is ignored. Nothing a normal user may do
//! clears that (tried on hardware: reconnecting, `usbip detach`, a user-mode
//! eject); restarting usbip-win2's virtual USB controller does, and the
//! devices come back with their old identities (ADR 0012).
//!
//! Restarting a device needs administrator rights, so the repair is a Task
//! Scheduler task that runs exactly one fixed command as SYSTEM:
//! `pnputil /restart-device /deviceid ROOT\USBIP_WIN2\UDE`. Setting it up
//! asks for administrator rights once ([`install_elevated`]); its access
//! list lets the user who set it up start it, so the engine can run it
//! ([`run`]) without asking again.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Name of the task in Task Scheduler.
pub const TASK_NAME: &str = "MixerCraft Repair Virtual Devices";
/// Hardware id of usbip-win2's virtual USB host controller.
pub const CONTROLLER_HARDWARE_ID: &str = r"ROOT\USBIP_WIN2\UDE";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Whether `sid` looks like a Windows security identifier (`S-1-5-21-...`).
#[must_use]
pub fn is_sid(sid: &str) -> bool {
    sid.len() < 200
        && sid.strip_prefix("S-1-").is_some_and(|rest| {
            !rest.is_empty() && rest.split('-').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// The task definition: runs the controller restart as SYSTEM; the user
/// `sid` may read and start it.
///
/// # Panics
/// If `sid` is not a security identifier (it goes into the access list).
#[must_use]
pub fn task_xml(sid: &str) -> String {
    assert!(is_sid(sid), "not a SID: {sid}");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Restarts the usbip-win2 virtual USB controller when Windows keeps stale copies of MixerCraft's virtual devices (after Fast Startup). Started by MixerCraft; runs only this command.</Description>
    <SecurityDescriptor>D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FRFX;;;{sid})</SecurityDescriptor>
  </RegistrationInfo>
  <Triggers />
  <Principals>
    <Principal id="System">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT2M</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="System">
    <Exec>
      <Command>%SystemRoot%\System32\pnputil.exe</Command>
      <Arguments>/restart-device /deviceid "{id}"</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        sid = xml_escape(sid),
        id = xml_escape(CONTROLLER_HARDWARE_ID),
    )
}

/// The SID in `whoami /user /fo csv /nh` output (`"desktop\name","S-1-5-..."`).
#[must_use]
pub fn sid_of_whoami(csv: &str) -> Option<String> {
    let line = csv.lines().find(|l| !l.trim().is_empty())?;
    let sid = line.rsplit(',').next()?.trim().trim_matches('"');
    is_sid(sid).then(|| sid.to_owned())
}

fn tool(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn run_tool(program: &str, args: &[&str]) -> Result<String, String> {
    let out = tool(program).args(args).output().map_err(|e| format!("cannot run {program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let mut message = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if message.is_empty() {
            message = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        }
        Err(format!("{program} failed: {message}"))
    }
}

/// The current user's SID.
///
/// # Errors
/// If `whoami` fails or prints no SID.
pub fn current_user_sid() -> Result<String, String> {
    let out = run_tool("whoami", &["/user", "/fo", "csv", "/nh"])?;
    sid_of_whoami(&out).ok_or_else(|| format!("whoami printed no SID: {}", out.trim()))
}

/// Whether the helper task is set up (and readable by this user).
#[must_use]
pub fn installed() -> bool {
    cfg!(windows) && run_tool("schtasks", &["/Query", "/TN", TASK_NAME]).is_ok()
}

/// Starts the helper task (returns once Task Scheduler has started it).
///
/// # Errors
/// If the task is missing or may not be started by this user.
pub fn run() -> Result<(), String> {
    run_tool("schtasks", &["/Run", "/TN", TASK_NAME]).map(|_| ())
}

/// Task Scheduler's result while the task runs (`SCHED_S_TASK_RUNNING`).
pub const STILL_RUNNING: i64 = 267_009;

/// Whether pnputil's exit code means the restart finishes only when Windows
/// restarts: 3010 ("reboot required"; seen on hardware when the audio engine
/// still had a device open), 50 (asked again while that is pending).
#[must_use]
pub const fn needs_windows_restart(code: i64) -> bool {
    code == 3010 || code == 50
}

/// Last run time (as Task Scheduler prints it) and last result from
/// `schtasks /Query /V /FO CSV /NH`. The first seven columns are fixed and
/// contain no quotes (later ones may, e.g. the command).
#[must_use]
pub fn parse_last_run(csv: &str) -> Option<(String, i64)> {
    let line = csv.lines().find(|l| l.starts_with('"'))?;
    let fields: Vec<&str> = line.trim_start_matches('"').splitn(8, "\",\"").collect();
    let (time, code) = (fields.get(5)?, fields.get(6)?);
    Some(((*time).to_owned(), code.trim().parse().ok()?))
}

fn last_run() -> Option<(String, i64)> {
    parse_last_run(&run_tool("schtasks", &["/Query", "/TN", TASK_NAME, "/V", "/FO", "CSV", "/NH"]).ok()?)
}

/// Starts the helper task and waits until it has finished; returns
/// pnputil's exit code (0: the controller restarted).
///
/// # Errors
/// If the task cannot be started or does not finish within `timeout`.
pub fn run_and_wait(timeout: Duration) -> Result<i64, String> {
    let before = last_run().map(|(time, _)| time);
    run()?;
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(500));
        if let Some((time, code)) = last_run()
            && Some(&time) != before.as_ref()
            && code != STILL_RUNNING
        {
            return Ok(code);
        }
    }
    Err("the repair helper did not finish in time".into())
}

/// Registers the helper task for the user `sid`. Needs administrator rights.
///
/// # Errors
/// An invalid SID, or Task Scheduler refusing.
pub fn install(sid: &str) -> Result<(), String> {
    if !is_sid(sid) {
        return Err(format!("not a SID: {sid}"));
    }
    // schtasks expects the XML file in UTF-16 with a byte-order mark.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(task_xml(sid).encode_utf16().flat_map(u16::to_le_bytes));
    let path = std::env::temp_dir().join(format!("mixercraft-repair-task-{}.xml", std::process::id()));
    std::fs::write(&path, bytes).map_err(|e| format!("cannot write the task definition: {e}"))?;
    let result = run_tool("schtasks", &["/Create", "/TN", TASK_NAME, "/XML", &path.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(&path);
    result.map(|_| ())
}

/// Removes the helper task. Needs administrator rights.
///
/// # Errors
/// Task Scheduler refusing (a missing task is not an error).
pub fn remove() -> Result<(), String> {
    if !installed() {
        return Ok(());
    }
    run_tool("schtasks", &["/Delete", "/TN", TASK_NAME, "/F"]).map(|_| ())
}

/// Runs `cli repair-helper <action>` as administrator (Windows asks the
/// user). `Ok(false)`: the user said no.
fn elevated(cli: &Path, action: &str) -> Result<bool, String> {
    let sid = current_user_sid()?;
    match mixercraft_winaudio::run_elevated(cli, &format!("repair-helper {action} --sid {sid}")) {
        Ok(Some(0)) => Ok(true),
        Ok(Some(code)) => Err(format!("setting up the repair helper failed (exit code {code})")),
        Ok(None) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// Sets up the helper task, asking for administrator rights. `cli` is the
/// `mixercraft` command-line program (it does the registration elevated).
/// `Ok(false)`: the user declined.
///
/// # Errors
/// If the elevated registration fails.
pub fn install_elevated(cli: &Path) -> Result<bool, String> {
    elevated(cli, "install")
}

/// Removes the helper task, asking for administrator rights.
///
/// # Errors
/// If the elevated removal fails.
pub fn remove_elevated(cli: &Path) -> Result<bool, String> {
    elevated(cli, "remove")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sids_are_checked() {
        assert!(is_sid("S-1-5-21-3623811015-3361044348-30300820-1013"));
        assert!(is_sid("S-1-5-18"));
        assert!(!is_sid("S-1-"));
        assert!(!is_sid("S-1-5-21-1)(A;;FA;;;WD"));
        assert!(!is_sid("desktop\\justin"));
    }

    #[test]
    fn whoami_output_gives_the_sid() {
        let csv = "\"desktop\\justin\",\"S-1-5-21-111-222-333-1001\"\r\n";
        assert_eq!(sid_of_whoami(csv).as_deref(), Some("S-1-5-21-111-222-333-1001"));
        assert_eq!(sid_of_whoami("garbage"), None);
    }

    #[test]
    fn task_results_are_read_from_the_csv() {
        // A German Windows (owner's PC); the command column contains quotes.
        let csv = r#""DESKTOP","\MixerCraft Repair Virtual Devices","Nicht zutreffend","Bereit","Interaktiv/Hintergrund","22.11.2026 13:37:14","50","Nicht zutreffend","%SystemRoot%\System32\pnputil.exe /restart-device /deviceid "ROOT\USBIP_WIN2\UDE"","Nicht zutreffend""#;
        assert_eq!(parse_last_run(csv), Some(("22.11.2026 13:37:14".into(), 50)));
        assert_eq!(parse_last_run("garbage"), None);
        assert!(needs_windows_restart(3010) && needs_windows_restart(50) && !needs_windows_restart(0));
    }

    #[test]
    fn the_task_runs_only_the_controller_restart() {
        let xml = task_xml("S-1-5-21-111-222-333-1001");
        assert!(xml.contains("<UserId>S-1-5-18</UserId>"), "runs as SYSTEM");
        assert!(xml.contains("(A;;FRFX;;;S-1-5-21-111-222-333-1001)"), "the user may start it");
        assert!(xml.contains(r"pnputil.exe</Command>"));
        assert!(xml.contains(r#"<Arguments>/restart-device /deviceid "ROOT\USBIP_WIN2\UDE"</Arguments>"#));
        assert!(!xml.contains("<LogonTrigger>"), "never runs by itself");
    }
}
