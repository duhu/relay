//! argv dispatch for the short-lived `relay` CLI (spec §7).
//!
//! Every command but `--devices` and `--displays` goes to the resident app
//! over the unix socket, starting it first if it is not there. Those two only
//! enumerate hardware, which any process may do, so they answer on their own
//! and work whether or not the app is running. stdout carries results only;
//! anything diagnostic goes to stderr (invariant 8).

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::app::{self, paths};
use crate::ipc::{self, Request, Response};

const EXIT_OK: i32 = 0;
const EXIT_FAILURE: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_NO_APP: i32 = 3;

/// How long to keep retrying the socket after starting the app.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_POLL: Duration = Duration::from_millis(250);

/// How long the CLI waits for an answer to a quick request (status, a dry
/// run, opening settings, reloading the config).
const QUICK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the CLI waits for a real switch: the core's DDC path retries 4
/// times with backoff (~8s each) before falling back, then makes two device
/// calls at up to 8s each — worst case is close to a minute.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(120);

const USAGE: &str = "\
Relay — hand the shared monitor and the Logitech keyboard/mouse to another Mac.

Usage:
  relay                       run the menu bar app (this is what login starts)
  relay switch <host|next>    switch to a host slot, or to the one after this machine
        --dry-run             print the plan instead of touching any hardware
        --no-spawn            fail instead of starting the app when it is not running
  relay status                print the resident app's state (also: relay --status)
  relay --settings            open the settings window
  relay --devices             list the HID devices this Mac can see
  relay --displays            list the external displays this Mac can drive
  relay --help                print this help

<host> is the 0-based HID++ slot as declared in config.json.
Exit codes: 0 done · 1 failed · 2 wrong usage · 3 the app is not running.
";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    /// No arguments: be the resident app.
    Run,
    Help,
    Devices,
    Displays,
    Status,
    Settings,
    Switch {
        target: Target,
        dry_run: bool,
        no_spawn: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Host(u8),
    Next,
}

/// Parses argv, runs the command and never returns.
pub fn dispatch() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match parse(&args) {
        Ok(command) => run(command),
        Err(message) => {
            eprintln!("relay: {message}");
            eprint!("{USAGE}");
            EXIT_USAGE
        }
    };
    std::process::exit(code);
}

fn parse(args: &[String]) -> Result<Command, String> {
    let Some(first) = args.first() else {
        return Ok(Command::Run);
    };
    match first.as_str() {
        "switch" => parse_switch(&args[1..]),
        "status" | "--status" => only(&args[1..], Command::Status),
        "--settings" => only(&args[1..], Command::Settings),
        "--devices" => only(&args[1..], Command::Devices),
        "--displays" => only(&args[1..], Command::Displays),
        "--help" | "-h" => Ok(Command::Help),
        other => Err(format!("unknown command '{other}'")),
    }
}

fn only(rest: &[String], command: Command) -> Result<Command, String> {
    match rest.first() {
        None => Ok(command),
        Some(extra) => Err(format!("unexpected argument '{extra}'")),
    }
}

fn parse_switch(args: &[String]) -> Result<Command, String> {
    let mut target = None;
    let mut dry_run = false;
    let mut no_spawn = false;

    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--no-spawn" => no_spawn = true,
            other if other.starts_with('-') => return Err(format!("unknown option '{other}'")),
            other if target.is_some() => return Err(format!("unexpected argument '{other}'")),
            "next" => target = Some(Target::Next),
            other => {
                let host = other.parse().map_err(|_| {
                    format!("'{other}' is not a host slot; give a number or 'next'")
                })?;
                target = Some(Target::Host(host));
            }
        }
    }

    let target = target.ok_or_else(|| "switch needs a host slot or 'next'".to_string())?;
    Ok(Command::Switch {
        target,
        dry_run,
        no_spawn,
    })
}

fn run(command: Command) -> i32 {
    match command {
        Command::Run => {
            app::run();
            EXIT_OK
        }
        Command::Help => {
            print!("{USAGE}");
            EXIT_OK
        }
        Command::Devices => {
            print_devices();
            EXIT_OK
        }
        Command::Displays => print_displays(),
        Command::Status => match fetch_status(false) {
            Ok(status) => {
                print_status(&status);
                EXIT_OK
            }
            Err(code) => code,
        },
        Command::Settings => match request(&Request::OpenSettings, false) {
            Ok(_) => EXIT_OK,
            Err(code) => code,
        },
        Command::Switch {
            target,
            dry_run,
            no_spawn,
        } => switch(target, dry_run, no_spawn),
    }
}

fn switch(target: Target, dry_run: bool, no_spawn: bool) -> i32 {
    let target = match target {
        Target::Host(index) => index,
        Target::Next => match next_target(no_spawn) {
            Ok(index) => index,
            Err(code) => return code,
        },
    };

    let data = match request(&Request::Switch { target, dry_run }, no_spawn) {
        Ok(data) => data,
        Err(code) => return code,
    };

    if dry_run {
        print_plan(data)
    } else {
        print_report(data)
    }
}

/// The host `switch next` means: the one after this machine in the configured
/// order. The resident app is the only place that knows the current config.
fn next_target(no_spawn: bool) -> Result<u8, i32> {
    let status = fetch_status(no_spawn)?;
    if !status.config_ok {
        let reason = status
            .config_error
            .unwrap_or_else(|| "there is no usable config".to_string());
        eprintln!("relay: {reason}");
        return Err(EXIT_FAILURE);
    }
    let Some(this_host) = status.this_host else {
        eprintln!("relay: the config does not say which host this machine is");
        return Err(EXIT_FAILURE);
    };
    next_host(&status.hosts, this_host).ok_or_else(|| {
        eprintln!("relay: host {this_host} is not among the declared hosts");
        EXIT_FAILURE
    })
}

/// The host after `this_host` in the configured order, wrapping around.
fn next_host(hosts: &[(u8, String)], this_host: u8) -> Option<u8> {
    let position = hosts.iter().position(|(index, _)| *index == this_host)?;
    hosts
        .get((position + 1) % hosts.len())
        .map(|(index, _)| *index)
}

fn fetch_status(no_spawn: bool) -> Result<StatusView, i32> {
    let data = request(&Request::Status, no_spawn)?;
    serde_json::from_value(data).map_err(|err| {
        eprintln!("relay: cannot read the status: {err}");
        EXIT_FAILURE
    })
}

/// Sends `request` to the resident app and unwraps a successful answer.
fn request(request: &Request, no_spawn: bool) -> Result<Value, i32> {
    match connect(request, no_spawn)? {
        Response::Ok { data } => Ok(data),
        Response::Error { message } => {
            eprintln!("relay: {message}");
            Err(EXIT_FAILURE)
        }
    }
}

fn connect(request: &Request, no_spawn: bool) -> Result<Response, i32> {
    let socket = paths::sock_path();
    let read_timeout = read_timeout_for(request);
    match ipc::call(&socket, request, read_timeout) {
        Ok(response) => return Ok(response),
        Err(err) if not_running(&err) => {}
        Err(err) => {
            eprintln!("relay: {}: {err}", socket.display());
            return Err(EXIT_FAILURE);
        }
    }

    if no_spawn {
        eprintln!("relay: the Relay app is not running");
        return Err(EXIT_NO_APP);
    }
    if let Err(err) = spawn_app() {
        eprintln!("relay: cannot start the Relay app: {err}");
        return Err(EXIT_NO_APP);
    }

    let deadline = Instant::now() + SPAWN_TIMEOUT;
    loop {
        std::thread::sleep(SPAWN_POLL);
        match ipc::call(&socket, request, read_timeout) {
            Ok(response) => return Ok(response),
            Err(err) if not_running(&err) && Instant::now() < deadline => continue,
            Err(err) => {
                eprintln!(
                    "relay: the Relay app did not answer within {}s: {err}",
                    SPAWN_TIMEOUT.as_secs()
                );
                return Err(EXIT_NO_APP);
            }
        }
    }
}

/// A real switch can take close to a minute (DDC retries plus two device
/// calls); everything else answers almost immediately.
fn read_timeout_for(request: &Request) -> Duration {
    match request {
        Request::Switch { dry_run: false, .. } => SWITCH_TIMEOUT,
        _ => QUICK_TIMEOUT,
    }
}

/// True when the socket says nobody is listening, rather than that the
/// conversation itself went wrong.
fn not_running(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Starts the resident app in the background.
///
/// An installed copy is launched through `open`, so launchservices owns it and
/// it outlives this CLI; a bare `cargo run` binary is detached by hand.
fn spawn_app() -> io::Result<()> {
    // `~/.local/bin/relay` is a symlink into the bundle and `current_exe` hands
    // back the path this process was invoked through, so resolve it first:
    // launching the bundle is what gives the app its resources and its TCC
    // identity.
    let exe = std::fs::canonicalize(std::env::current_exe()?)?;

    if let Some(bundle) = app_bundle_of(&exe) {
        let status = std::process::Command::new("open")
            .arg("-g")
            .arg("-a")
            .arg(&bundle)
            .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "open -g -a {} exited with {status}",
                bundle.display()
            )))
        };
    }

    let mut command = std::process::Command::new(&exe);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe, which is all a `pre_exec` hook
    // between fork and exec may call.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()?;
    Ok(())
}

/// `/Applications/Relay.app` for `/Applications/Relay.app/Contents/MacOS/relay`.
fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

fn print_devices() {
    for (vid, pid, name) in relay_core::trigger::presence::list_hid_devices() {
        println!("{vid:04x}:{pid:04x}  {name}");
    }
}

/// `--displays`, the display counterpart of [`print_devices`].
///
/// Discovery is async, so it needs a runtime; the CLI has none of its own and
/// a current-thread one is enough for a single `spawn_blocking`. Without one
/// the enumeration cannot run at all, which is a failure, not an empty list.
fn print_displays() -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("relay: cannot enumerate displays: {err}");
            return EXIT_FAILURE;
        }
    };
    for display in runtime.block_on(relay_core::display::ddc::list_displays()) {
        println!("{}  {}", display.edid_uuid, display.name);
    }
    EXIT_OK
}

fn print_status(status: &StatusView) {
    println!("state: {}", status.state);
    println!("config_ok: {}", status.config_ok);
    if let Some(error) = &status.config_error {
        println!("config_error: {error}");
    }
    match status.this_host {
        Some(host) => println!("this_host: {host}"),
        None => println!("this_host: -"),
    }
    let hosts: Vec<String> = status
        .hosts
        .iter()
        .map(|(index, name)| format!("{index} {name}"))
        .collect();
    if hosts.is_empty() {
        println!("hosts: -");
    } else {
        println!("hosts: {}", hosts.join(", "));
    }
    println!("input_monitoring: {}", status.input_monitoring);
}

fn print_plan(data: Value) -> i32 {
    let plan: PlanView = match serde_json::from_value(data) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("relay: cannot read the plan: {err}");
            return EXIT_FAILURE;
        }
    };

    println!("plan → host {}", plan.target);
    for display in &plan.displays {
        println!("display {} → input {}", display.name, display.input);
    }
    for device in &plan.devices {
        println!("device {device}");
    }
    EXIT_OK
}

fn print_report(data: Value) -> i32 {
    let report: ReportView = match serde_json::from_value(data) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("relay: cannot read the report: {err}");
            return EXIT_FAILURE;
        }
    };

    for step in &report.steps {
        let mark = if step.ok { "ok" } else { "FAIL" };
        println!("{mark} {} — {} ({}ms)", step.what, step.detail, step.ms);
    }
    if report.ok {
        EXIT_OK
    } else {
        EXIT_FAILURE
    }
}

/// The parts of `relay_core::runtime::Status` the CLI prints. The core type is
/// serialize-only, so the CLI reads the wire form back into its own struct.
#[derive(Debug, Deserialize)]
struct StatusView {
    state: String,
    config_ok: bool,
    config_error: Option<String>,
    this_host: Option<u8>,
    hosts: Vec<(u8, String)>,
    input_monitoring: bool,
}

#[derive(Debug, Deserialize)]
struct PlanView {
    target: u8,
    displays: Vec<PlanDisplay>,
    devices: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PlanDisplay {
    name: String,
    input: u8,
}

#[derive(Debug, Deserialize)]
struct ReportView {
    steps: Vec<ReportStep>,
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct ReportStep {
    what: String,
    ok: bool,
    detail: String,
    ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    fn parsed(args: &[&str]) -> Command {
        parse(&argv(args)).expect("the arguments parse")
    }

    #[test]
    fn no_arguments_run_the_resident_app() {
        assert_eq!(parsed(&[]), Command::Run);
    }

    #[test]
    fn status_has_a_flag_form_and_a_subcommand_form() {
        assert_eq!(parsed(&["status"]), Command::Status);
        assert_eq!(parsed(&["--status"]), Command::Status);
    }

    #[test]
    fn the_remaining_commands_parse() {
        assert_eq!(parsed(&["--settings"]), Command::Settings);
        assert_eq!(parsed(&["--devices"]), Command::Devices);
        assert_eq!(parsed(&["--displays"]), Command::Displays);
        assert_eq!(parsed(&["--help"]), Command::Help);
        assert_eq!(parsed(&["-h"]), Command::Help);
    }

    #[test]
    fn switch_takes_a_host_slot_and_its_flags_in_any_order() {
        assert_eq!(
            parsed(&["switch", "1"]),
            Command::Switch {
                target: Target::Host(1),
                dry_run: false,
                no_spawn: false,
            }
        );
        assert_eq!(
            parsed(&["switch", "--dry-run", "2", "--no-spawn"]),
            Command::Switch {
                target: Target::Host(2),
                dry_run: true,
                no_spawn: true,
            }
        );
        assert_eq!(
            parsed(&["switch", "next"]),
            Command::Switch {
                target: Target::Next,
                dry_run: false,
                no_spawn: false,
            }
        );
    }

    #[test]
    fn misuse_is_reported_rather_than_guessed_at() {
        for args in [
            vec!["switch"],
            vec!["switch", "left"],
            vec!["switch", "1", "2"],
            vec!["switch", "1", "--wat"],
            vec!["switch", "999"],
            vec!["status", "now"],
            vec!["--devices", "all"],
            vec!["--displays", "all"],
            vec!["wizard"],
        ] {
            assert!(
                parse(&argv(&args)).is_err(),
                "{args:?} should not have parsed"
            );
        }
    }

    fn hosts() -> Vec<(u8, String)> {
        vec![
            (1, "Bam.Work".to_string()),
            (2, "Bam.Mini".to_string()),
            (3, "Bam.Spare".to_string()),
        ]
    }

    #[test]
    fn next_host_follows_the_configured_order() {
        assert_eq!(next_host(&hosts(), 1), Some(2));
        assert_eq!(next_host(&hosts(), 2), Some(3));
    }

    #[test]
    fn next_host_wraps_around() {
        assert_eq!(next_host(&hosts(), 3), Some(1));
        assert_eq!(next_host(&[(1, "only".to_string())], 1), Some(1));
    }

    #[test]
    fn next_host_needs_this_machine_among_the_hosts() {
        assert_eq!(next_host(&hosts(), 0), None);
        assert_eq!(next_host(&[], 1), None);
    }

    #[test]
    fn the_cli_reads_the_plan_shape_the_app_sends() {
        let plan = relay_core::plan::SwitchPlan {
            target: 2,
            displays: vec![("AOC U2790R3B".to_string(), 18)],
            devices: vec![relay_core::types::DeviceId("046d:b023".to_string())],
            manual: true,
        };

        let view: PlanView =
            serde_json::from_value(crate::app::plan_json(&plan)).expect("the plan reads back");

        assert_eq!(view.target, 2);
        assert_eq!(view.displays[0].name, "AOC U2790R3B");
        assert_eq!(view.displays[0].input, 18);
        assert_eq!(view.devices, ["046d:b023"]);
    }

    #[test]
    fn the_cli_reads_the_status_the_core_publishes() {
        let status = relay_core::runtime::Status {
            state: "Idle".to_string(),
            config_ok: true,
            config_error: None,
            this_host: Some(1),
            hosts: vec![(1, "Bam.Work".to_string()), (2, "Bam.Mini".to_string())],
            input_monitoring: true,
            last_report: None,
            language: "zh-Hans",
        };

        let view: StatusView =
            serde_json::from_value(serde_json::to_value(&status).expect("serializes"))
                .expect("the status reads back");

        assert_eq!(view.state, "Idle");
        assert!(view.config_ok);
        assert_eq!(view.config_error, None);
        assert_eq!(view.this_host, Some(1));
        assert_eq!(view.hosts, status.hosts);
        assert!(view.input_monitoring);
    }

    #[test]
    fn the_cli_reads_the_report_the_core_returns() {
        let report = relay_core::executor::SwitchReport {
            target: 2,
            steps: vec![relay_core::executor::StepResult {
                what: "display AOC U2790R3B".to_string(),
                ok: false,
                detail: "display did not answer in time".to_string(),
                ms: 1200,
            }],
            ok: false,
        };

        let view: ReportView =
            serde_json::from_value(serde_json::to_value(&report).expect("serializes"))
                .expect("the report reads back");

        assert!(!view.ok);
        assert_eq!(view.steps[0].what, "display AOC U2790R3B");
        assert!(!view.steps[0].ok);
        assert_eq!(view.steps[0].detail, "display did not answer in time");
        assert_eq!(view.steps[0].ms, 1200);
    }

    #[test]
    fn an_installed_binary_resolves_to_its_bundle() {
        assert_eq!(
            app_bundle_of(Path::new(
                "/Applications/Relay-dev.app/Contents/MacOS/relay"
            )),
            Some(PathBuf::from("/Applications/Relay-dev.app"))
        );
    }

    #[test]
    fn a_loose_binary_has_no_bundle() {
        for exe in [
            "/Users/me/dev-relay/target/debug/relay",
            "/usr/local/bin/relay",
            "/Applications/Relay-dev.app/Contents/Resources/relay",
            "/tmp/Contents/MacOS/relay",
        ] {
            assert_eq!(app_bundle_of(Path::new(exe)), None, "{exe}");
        }
    }
}
