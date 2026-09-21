//! Everything wired together: the loop that owns the state machine.
//!
//! One tokio task holds the [`Coordinator`], the current [`Config`] and the
//! timer; presence events, config changes and handle requests all arrive as
//! messages, so there is a single place where the hardware can be touched and
//! the debounce/cooldown cannot be bypassed (`AGENTS.md`).
//!
//! Nothing above this module talks to the state machine directly: the UI, the
//! hotkeys and the CLI all go through [`CoreHandle`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::RecommendedWatcher;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::config::{Config, ConfigError, Language};
use crate::config_watch;
use crate::coordinator::{Action, Coordinator, Event, State};
use crate::device::logitech::LogitechHidpp;
use crate::device::HostSwitchable;
use crate::display::ddc::DdcDisplay;
use crate::display::DisplayInput;
use crate::executor::{run_plan, StepResult, SwitchReport};
use crate::log::{LogBuffer, LogEntry};
use crate::permissions;
use crate::plan::{build_plan, PlanError, SwitchPlan};
use crate::trigger::{presence, PresenceTracker, RawHidEvent};
use crate::types::{HostIndex, TriggerEvent};

/// Everything the tray and the settings window show.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Status {
    /// `Idle`, `Confirming`, `Switching`, `Cooldown` or `Unconfigured`.
    pub state: String,
    pub config_ok: bool,
    pub config_error: Option<String>,
    pub this_host: Option<HostIndex>,
    pub hosts: Vec<(HostIndex, String)>,
    pub input_monitoring: bool,
    pub last_report: Option<SwitchReport>,
    /// The concrete language the window and the tray must speak, `"zh-Hans"`
    /// or `"en"`. `options.language` may say `"auto"`; this never does, so the
    /// two UIs cannot resolve "follow the system" differently.
    pub language: &'static str,
}

impl Status {
    /// What the status looks like before the first config has been read.
    fn unconfigured() -> Self {
        Status {
            state: STATE_UNCONFIGURED.to_string(),
            config_ok: false,
            config_error: None,
            this_host: None,
            hosts: Vec::new(),
            input_monitoring: false,
            last_report: None,
            // There is no config to read a preference out of yet, so the
            // system's own preference is the only answer there is.
            language: Language::Auto.resolve(),
        }
    }
}

const STATE_UNCONFIGURED: &str = "Unconfigured";

/// How long the pull decision waits for the monitor.
///
/// The read itself may take up to [`DdcDisplay`]'s own eight seconds, a budget
/// sized for a retried *write*; but this one is awaited inside the loop, so
/// whatever it waits, the core waits: no switch report, no config reload and
/// no `status()` is served meanwhile. (A HID event does get through — it
/// abandons the read outright, see [`Runtime::ask_the_screen`].) A monitor that is
/// asleep or whose DDC channel has wedged is exactly the situation in which
/// the user walks up and presses Easy-Switch, so the answer is given a second
/// and a half and no more. A healthy read takes about 60 ms; a read that has
/// to be retried because the channel is not ready yet takes about 540 ms
/// (`DDC_READ_RETRY_WAIT` in the display layer) and must still fit. A late
/// answer is worth no more than none: either way the fallback rule decides.
const DECISION_READ_TIMEOUT: Duration = Duration::from_millis(1_500);

/// How [`Runtime::ask_the_screen`] ended.
enum Asked {
    /// Nobody interrupted: the reading, or `None` when the monitor would not
    /// say (or was not there to ask, or ran out of budget).
    Answered(Option<bool>),
    /// A device event arrived first and was handled instead of the read;
    /// `changed` is what handling it reported to the status channel.
    Interrupted { changed: bool },
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("a switch is already running")]
    Busy,
    #[error("there is no usable config")]
    Unconfigured,
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("the core has stopped")]
    Stopped,
}

/// What a [`CoreHandle`] asks the loop to do; every variant carries its reply.
enum Request {
    Status(oneshot::Sender<Status>),
    Switch {
        target: HostIndex,
        source: &'static str,
        reply: oneshot::Sender<Result<SwitchReport, CoreError>>,
    },
    DryRun {
        target: HostIndex,
        reply: oneshot::Sender<Result<SwitchPlan, CoreError>>,
    },
    Reload(oneshot::Sender<Result<(), CoreError>>),
}

/// How the executor's displays and devices are built from a config.
///
/// Production uses the native adapters ([`DdcDisplay`], [`LogitechHidpp`]);
/// tests substitute fakes. This is a test seam, not public API.
#[doc(hidden)]
#[derive(Clone)]
pub struct Factories {
    pub displays: DisplayFactory,
    pub devices: DeviceFactory,
}

#[doc(hidden)]
pub type DisplayFactory = Arc<dyn Fn(&Config) -> Vec<Arc<dyn DisplayInput>> + Send + Sync>;

#[doc(hidden)]
pub type DeviceFactory = Arc<dyn Fn(&Config) -> Vec<Arc<dyn HostSwitchable>> + Send + Sync>;

pub struct Core;

impl Core {
    /// Starts the core with the real trigger source and config watcher.
    ///
    /// Must be called from inside a tokio runtime; the loop runs until every
    /// [`CoreHandle`] is dropped.
    pub fn start(config_path: PathBuf, log: LogBuffer) -> CoreHandle {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (config_tx, config_rx) = mpsc::unbounded_channel();

        // The HID matcher is set once, from whatever the config says now; an
        // unreadable config watches every device and lets the tracker filter.
        let watched = Config::load(&config_path)
            .map(|cfg| cfg.devices.iter().filter_map(|d| d.id.vid_pid()).collect())
            .unwrap_or_default();
        if let Err(err) = presence::start_watcher(watched, raw_tx) {
            tracing::error!(error = %err, "the HID watcher did not start; presence switching is off");
        }

        let watcher = match config_watch::watch(config_path.clone(), config_tx) {
            Ok(watcher) => Some(Arc::new(watcher)),
            Err(err) => {
                tracing::error!(error = %err, "config changes will not be noticed");
                None
            }
        };

        let mut handle = Self::start_with(config_path, log, raw_rx, config_rx, None);
        handle.config_watcher = watcher;
        handle
    }

    /// [`Core::start`] with the trigger and config channels supplied by the
    /// caller, so tests can drive the loop without any hardware.
    #[doc(hidden)]
    pub fn start_with(
        config_path: PathBuf,
        log: LogBuffer,
        raw_rx: mpsc::UnboundedReceiver<RawHidEvent>,
        config_rx: mpsc::UnboundedReceiver<()>,
        factories: Option<Factories>,
    ) -> CoreHandle {
        let (requests_tx, requests_rx) = mpsc::unbounded_channel();
        let (status_tx, status_rx) = watch::channel(Status::unconfigured());
        let (done_tx, done_rx) = mpsc::unbounded_channel();

        let mut runtime = Runtime {
            matcher_vendors: matcher_vendors(&config_path),
            config_path,
            factories,
            cfg: None,
            coordinator: None,
            config_error: None,
            pending_config: None,
            pending_removal: false,
            switch_in_flight: false,
            tracker: PresenceTracker::new(Vec::new()),
            deadline: None,
            pending_manual: None,
            last_report: None,
            status_tx,
            done_tx,
        };
        // A bad config is a state, not a failure to start: the UI shows it.
        let _ = runtime.reload();
        runtime.publish();

        tokio::spawn(runtime.run(raw_rx, config_rx, requests_rx, done_rx));

        CoreHandle {
            requests: requests_tx,
            status: status_rx,
            log,
            config_watcher: None,
        }
    }
}

/// A cheap, cloneable way to talk to the running core.
#[derive(Clone)]
pub struct CoreHandle {
    requests: mpsc::UnboundedSender<Request>,
    status: watch::Receiver<Status>,
    log: LogBuffer,
    /// Kept alive here because dropping it stops the config watch.
    config_watcher: Option<Arc<RecommendedWatcher>>,
}

impl CoreHandle {
    pub async fn status(&self) -> Status {
        match self.ask(Request::Status).await {
            Ok(status) => status,
            // The loop is gone; the last published status is the truth.
            Err(_) => self.status.borrow().clone(),
        }
    }

    /// Switches to `target` now, skipping the debounce; `source` is for the log.
    pub async fn switch(
        &self,
        target: HostIndex,
        source: &'static str,
    ) -> Result<SwitchReport, CoreError> {
        self.ask(|reply| Request::Switch {
            target,
            source,
            reply,
        })
        .await?
    }

    /// The plan a manual switch to `target` would run, without running it.
    pub async fn dry_run(&self, target: HostIndex) -> Result<SwitchPlan, CoreError> {
        self.ask(|reply| Request::DryRun { target, reply }).await?
    }

    /// Re-reads the config file now, instead of waiting for the watcher.
    pub async fn reload(&self) -> Result<(), CoreError> {
        self.ask(Request::Reload).await?
    }

    /// Status updates for the tray; the current value is already in it.
    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// The recent log entries, for the settings window's Log view.
    pub fn logs(&self) -> Vec<LogEntry> {
        self.log.snapshot()
    }

    async fn ask<T>(
        &self,
        request: impl FnOnce(oneshot::Sender<T>) -> Request,
    ) -> Result<T, CoreError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.requests
            .send(request(reply_tx))
            .map_err(|_| CoreError::Stopped)?;
        reply_rx.await.map_err(|_| CoreError::Stopped)
    }
}

/// The loop's own state. Only the loop task ever touches it.
struct Runtime {
    config_path: PathBuf,
    factories: Option<Factories>,
    /// The config the coordinator is running on; `None` while it is unusable.
    cfg: Option<Config>,
    coordinator: Option<Coordinator>,
    config_error: Option<String>,
    /// A newer config that may only be installed once the core is idle.
    pending_config: Option<Config>,
    /// A failed reload that may only take the config away once the core is
    /// idle; the counterpart of `pending_config`.
    pending_removal: bool,
    /// Set from [`Runtime::start_switch`] until the report comes back. The
    /// coordinator's `Switching` is not enough: a reload may replace it, and
    /// two overlapping switches would fight over the same hardware.
    switch_in_flight: bool,
    /// The vendor ids the HID matcher was built with at start; empty means it
    /// matches every device (see [`presence::start_watcher`]).
    matcher_vendors: Vec<u16>,
    tracker: PresenceTracker,
    /// The one timer the coordinator asked for, if any.
    deadline: Option<Instant>,
    /// The `switch()` caller waiting for the running switch to finish.
    pending_manual: Option<oneshot::Sender<Result<SwitchReport, CoreError>>>,
    last_report: Option<SwitchReport>,
    status_tx: watch::Sender<Status>,
    done_tx: mpsc::UnboundedSender<SwitchReport>,
}

impl Runtime {
    async fn run(
        mut self,
        mut raw_rx: mpsc::UnboundedReceiver<RawHidEvent>,
        mut config_rx: mpsc::UnboundedReceiver<()>,
        mut requests_rx: mpsc::UnboundedReceiver<Request>,
        mut done_rx: mpsc::UnboundedReceiver<SwitchReport>,
    ) {
        loop {
            // Biased so that hardware and timing are always handled before
            // questions about them; a status query never overtakes an event.
            //
            // `None` is the timer: firing it may have to ask the monitor, and
            // that read races `raw_rx`, which this `select!` is still holding.
            // So the branch only reports that the deadline passed and the work
            // happens once the macro has let go of the receiver.
            let changed = tokio::select! {
                biased;

                Some(raw) = raw_rx.recv() => Some(self.handle_raw(raw).await),
                Some(report) = done_rx.recv() => Some(self.finish_switch(report)),
                Some(()) = config_rx.recv() => {
                    let _ = self.reload();
                    Some(true)
                }
                () = sleep_until(self.deadline), if self.deadline.is_some() => None,
                request = requests_rx.recv() => match request {
                    // Every handle is gone: nothing can ask for a switch again.
                    None => break,
                    Some(request) => Some(self.handle_request(request)),
                },
            };
            let changed = match changed {
                Some(changed) => changed,
                None => self.fire_timer(&mut raw_rx).await,
            };

            let installed = self.install_pending_config();
            // `input_monitoring` is read live, so it can change without any
            // event of ours: comparing this iteration's value against the last
            // published one is what makes the tray notice a permission granted
            // (or revoked) in System Settings. One `status()` per wakeup, so
            // the permission is still checked exactly once either way.
            let status = self.status();
            let permission_flipped =
                status.input_monitoring != self.status_tx.borrow().input_monitoring;
            if changed || installed || permission_flipped {
                self.status_tx.send_replace(status);
            }
        }
        tracing::info!("the core loop stopped");
    }

    /// Feeds one HID node event through the tracker. Returns whether the
    /// status may have changed.
    async fn handle_raw(&mut self, raw: RawHidEvent) -> bool {
        let Some(trigger) = self.tracker.feed(raw) else {
            return false;
        };
        tracing::info!(trigger = ?trigger, "presence changed");
        self.handle_event(Event::Trigger(trigger))
    }

    /// Would the coordinator use a reading if it had one? Only the expiry of a
    /// debounce that a trigger device's arrival started.
    ///
    /// The question is deliberately asked this late. The arrival itself is
    /// about a second after the *other* Mac wrote the monitor's input, and in
    /// that window the display is still re-syncing: it answers with the input
    /// it is leaving, or not at all (the write path sees the same thing — a
    /// first attempt that finds no display, and a retry that works). By the
    /// end of the debounce it has settled, and the answer is the freshest one
    /// the decision can have.
    fn pull_needs_the_screen(&self) -> bool {
        let Some(cfg) = self.cfg.as_ref() else {
            return false;
        };
        // The coordinator's own guards — not a trigger, pull turned off —
        // already ran when the debounce was armed, so an arrival debounce that
        // is still standing is one whose verdict wants a reading.
        matches!(
            self.coordinator.as_ref().map(Coordinator::state),
            Some(State::Confirming {
                from_arrival: true,
                ..
            })
        ) && cfg.options.pull_on_arrival
    }

    /// What the decision read needs to run on its own: the display to ask and
    /// the input code this host is mapped to, or `None` when there is nothing
    /// to ask.
    ///
    /// "The screen is here" means the **first** configured display reports the
    /// input source this host is mapped to. One display is all a Mac can be
    /// judged by today; deciding it from several of them is a question of its
    /// own, left for when Relay grows a second monitor.
    ///
    /// The pieces are taken out of `self` here so that the read below borrows
    /// nothing: it has to be raced against the trigger channel, and whoever
    /// wins that race is then handled with `&mut self`.
    fn decision_read(&self) -> Option<(Arc<dyn DisplayInput>, u8)> {
        // Every one of these is a reason the arrival decides without a
        // reading, and spec §5 promises that every ask is readable in the log
        // afterwards — including the asks that never happened.
        let cfg = self.cfg.as_ref()?;
        let Some(configured) = cfg.displays.first() else {
            tracing::debug!("no display is configured; the arrival decides without a reading");
            return None;
        };
        let Some(wanted) = configured.input_for(cfg.this_host) else {
            tracing::debug!(
                host = cfg.this_host,
                display = configured.name,
                "this host has no input on the display; the arrival decides without a reading",
            );
            return None;
        };
        let Some(adapter) = self.displays(cfg).into_iter().next() else {
            tracing::debug!("no display adapter was built; the arrival decides without a reading");
            return None;
        };
        Some((adapter, wanted))
    }

    /// Asks the monitor where the screen is, racing the question against the
    /// trigger channel: whoever answers first wins.
    ///
    /// The read is awaited inside the event loop, so while it runs nothing
    /// else is served. A trigger device *leaving while we are asking* is
    /// exactly the case that must not lose the race: the user pressed
    /// Easy-Switch straight on to the other Mac, and a `DeviceLeft` queued
    /// behind the read would only be seen once `TimerFired` had already put
    /// the coordinator into `Switching` towards this host — where a leave is
    /// no longer a departure, only a cleared return. The screen would be
    /// pulled here and kept here while the keyboard is over there.
    ///
    /// So the first raw HID event abandons the read. The news is worth more
    /// than the answer, and without an answer the arrival simply falls back to
    /// `last_target` (spec §5). When nothing interrupts, the read still gets
    /// [`DECISION_READ_TIMEOUT`] and no more.
    async fn ask_the_screen(&mut self, raw_rx: &mut mpsc::UnboundedReceiver<RawHidEvent>) -> Asked {
        let Some((display, wanted)) = self.decision_read() else {
            return Asked::Answered(None);
        };
        let read = tokio::time::timeout(DECISION_READ_TIMEOUT, display.current_input());
        tokio::pin!(read);
        let showing = tokio::select! {
            biased;

            Some(raw) = raw_rx.recv() => {
                tracing::info!(
                    "a device moved while the monitor was being asked; the read is dropped",
                );
                return Asked::Interrupted {
                    changed: self.handle_raw(raw).await,
                };
            }
            showing = &mut read => match showing {
                Ok(showing) => showing,
                Err(_elapsed) => {
                    tracing::info!(
                        budget_ms = DECISION_READ_TIMEOUT.as_millis() as u64,
                        "the monitor did not answer in time; deciding without it"
                    );
                    None
                }
            },
        };
        let here = showing.map(|showing| showing == wanted);
        // At `info`, because this one line is what makes a skipped (or taken)
        // pull readable in the log afterwards.
        tracing::info!(
            read = ?showing,
            wanted,
            here = ?here,
            "asked the monitor where the screen is"
        );
        Asked::Answered(here)
    }

    async fn fire_timer(&mut self, raw_rx: &mut mpsc::UnboundedReceiver<RawHidEvent>) -> bool {
        self.deadline = None;
        // The one place the answer is used, so the one place we ask: reading
        // costs the display about 60 ms, and every other path (a leave, the
        // end of a cooldown, a finished switch, any hotplug of a device we do
        // not follow) decides without it.
        if !self.pull_needs_the_screen() {
            return self.handle_event(Event::TimerFired);
        }
        let here = match self.ask_the_screen(raw_rx).await {
            Asked::Answered(here) => here,
            Asked::Interrupted { changed } => {
                // The event we handled instead may have moved the machine on —
                // a leave of its own, or straight into `Switching`. Its own
                // action (a fresh timer, or none) stands; this expiry is spent,
                // and the `TimerFired` it was about would only be ignored.
                if !self.pull_needs_the_screen() {
                    return changed;
                }
                // Still the same arrival debounce (another device's hotplug,
                // say): decide it with no reading, as a mute monitor would.
                None
            }
        };
        if let Some(coordinator) = self.coordinator.as_mut() {
            coordinator.observe_screen(here);
        }
        self.handle_event(Event::TimerFired)
    }

    fn handle_event(&mut self, event: Event) -> bool {
        let now = now();
        let Some(coordinator) = self.coordinator.as_mut() else {
            tracing::debug!(event = ?event, "no usable config; the event is dropped");
            return false;
        };
        let action = coordinator.handle(event, now);
        self.apply(action);
        true
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::ArmTimer(at) => self.deadline = Some(Instant::from_std(at)),
            Action::CancelTimer => self.deadline = None,
            Action::StartSwitch { target, manual } => self.start_switch(target, manual),
            Action::Ignore(reason) => tracing::debug!(reason, "no action"),
        }
    }

    /// Runs the plan for `target` on a task of its own; the report comes back
    /// through `done_tx`.
    fn start_switch(&mut self, target: HostIndex, manual: bool) {
        // Cleared only by the report, including the ones `fail_switch` fakes.
        self.switch_in_flight = true;
        // Both of these are ruled out by config validation and by the fact
        // that a coordinator only exists with a config behind it. They are
        // still reported as a failed switch rather than dropped: a `Switching`
        // state that nothing ever finishes would never cool down.
        let Some(cfg) = self.cfg.clone() else {
            self.fail_switch(target, "there is no usable config".to_string());
            return;
        };
        let plan = match build_plan(&cfg, target, manual) {
            Ok(plan) => plan,
            Err(err) => {
                self.fail_switch(target, err.to_string());
                return;
            }
        };

        tracing::info!(target, manual, "switching");
        let displays = self.displays(&cfg);
        let devices = self.devices(&cfg);
        let retries = u32::from(cfg.timing.ddc_retries);
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            // The plan runs on a task of its own so that this one can watch it:
            // an adapter that panics must still produce a report, or the
            // coordinator stays in `Switching` and never cools down.
            let running =
                tokio::spawn(async move { run_plan(&plan, &displays, &devices, retries).await });
            let report = match running.await {
                Ok(report) => report,
                Err(err) => {
                    tracing::error!(target, error = %err, "the switch task died");
                    failed_report(target, format!("the switch task died: {err}"))
                }
            };
            // The loop is gone only if the whole core shut down mid-switch.
            let _ = done.send(report);
        });
    }

    /// Reports a switch that never started, so the state machine still runs
    /// its course back to `Idle`.
    fn fail_switch(&mut self, target: HostIndex, detail: String) {
        tracing::error!(target, detail, "the switch could not start");
        let _ = self.done_tx.send(failed_report(target, detail));
    }

    fn finish_switch(&mut self, report: SwitchReport) -> bool {
        self.switch_in_flight = false;
        tracing::info!(target = report.target, ok = report.ok, "switch finished");
        if let Some(reply) = self.pending_manual.take() {
            let _ = reply.send(Ok(report.clone()));
        }
        self.last_report = Some(report);
        self.handle_event(Event::SwitchFinished)
    }

    fn handle_request(&mut self, request: Request) -> bool {
        match request {
            Request::Status(reply) => {
                let _ = reply.send(self.status());
                false
            }
            Request::Switch {
                target,
                source,
                reply,
            } => {
                self.manual_switch(target, source, reply);
                true
            }
            Request::DryRun { target, reply } => {
                let plan = match &self.cfg {
                    Some(cfg) => build_plan(cfg, target, true).map_err(CoreError::Plan),
                    None => Err(CoreError::Unconfigured),
                };
                let _ = reply.send(plan);
                false
            }
            Request::Reload(reply) => {
                let _ = reply.send(self.reload());
                true
            }
        }
    }

    fn manual_switch(
        &mut self,
        target: HostIndex,
        source: &'static str,
        reply: oneshot::Sender<Result<SwitchReport, CoreError>>,
    ) {
        let (Some(cfg), Some(coordinator)) = (self.cfg.as_ref(), self.coordinator.as_mut()) else {
            let _ = reply.send(Err(CoreError::Unconfigured));
            return;
        };
        // Refuse an impossible target before the state machine commits to it.
        if let Err(err) = build_plan(cfg, target, true) {
            let _ = reply.send(Err(CoreError::Plan(err)));
            return;
        }

        let action = coordinator.handle(
            Event::Trigger(TriggerEvent::Manual { target, source }),
            now(),
        );
        match action {
            Action::StartSwitch { target, manual } => {
                self.pending_manual = Some(reply);
                self.start_switch(target, manual);
            }
            other => {
                tracing::info!(source, target, action = ?other, "manual switch refused");
                let _ = reply.send(Err(CoreError::Busy));
            }
        }
    }

    /// Re-reads the config file and installs it when the core is idle.
    fn reload(&mut self) -> Result<(), CoreError> {
        match load(&self.config_path) {
            Ok(cfg) => {
                tracing::info!(path = %self.config_path.display(), "config loaded");
                self.config_error = None;
                self.pending_removal = false;
                self.warn_about_unmatched_vendors(&cfg);

                // The HID matcher stays as it is; only which devices count.
                // The tracker keeps the nodes it has already seen, so a device
                // that arrived before this config still counts as present.
                self.tracker
                    .set_watched(cfg.devices.iter().map(|d| d.id.clone()).collect());

                if self.is_idle() {
                    self.install(cfg);
                } else {
                    // Swapping the config under a running switch would change
                    // the cooldown out from under it.
                    tracing::info!("the new config waits for the switch to finish");
                    self.pending_config = Some(cfg);
                }
                Ok(())
            }
            Err(err) => {
                let message = err.to_string();
                tracing::warn!(error = %message, "unusable config; switching is off");
                self.config_error = Some(message.clone());
                self.pending_config = None;
                if !self.is_idle() {
                    // Dropping (or replacing) the coordinator now would either
                    // strand a running switch in `Switching` (its report would
                    // find nothing to finish, so the cooldown would never
                    // happen and the next config could start a second,
                    // overlapping switch) or, during `Cooldown`, hand out a
                    // fresh `Idle` coordinator on the next valid reload and let
                    // a switch through without the cooldown. Confirming is
                    // included for the same reason: nothing outside `Idle` may
                    // see the coordinator swapped out from under it.
                    tracing::info!("the config is dropped once the coordinator is idle");
                    self.pending_removal = true;
                } else {
                    self.cfg = None;
                    self.coordinator = None;
                    self.deadline = None;
                }
                Err(CoreError::Config(message))
            }
        }
    }

    /// The matcher is built once, at start, from the vendors the config named
    /// then; a device of another vendor added later is invisible until restart.
    fn warn_about_unmatched_vendors(&self, cfg: &Config) {
        if self.matcher_vendors.is_empty() {
            // No vendors means the matcher takes every HID device.
            return;
        }
        for device in &cfg.devices {
            let Some((vid, _)) = device.id.vid_pid() else {
                continue;
            };
            if !self.matcher_vendors.contains(&vid) {
                tracing::warn!(
                    device = %device.id,
                    vendor = format!("{vid:04x}"),
                    "the HID matcher was built without this vendor; restart to watch this device"
                );
            }
        }
    }

    fn install(&mut self, cfg: Config) {
        self.coordinator = Some(Coordinator::new(cfg.clone()));
        self.cfg = Some(cfg);
    }

    /// Applies whatever the last reload deferred: a newer config, or the
    /// removal of an unusable one.
    fn install_pending_config(&mut self) -> bool {
        if !self.is_idle() {
            return false;
        }
        if self.pending_removal {
            self.pending_removal = false;
            self.cfg = None;
            self.coordinator = None;
            self.deadline = None;
            return true;
        }
        match self.pending_config.take() {
            Some(cfg) => {
                self.install(cfg);
                true
            }
            None => false,
        }
    }

    fn is_idle(&self) -> bool {
        !self.switch_in_flight
            && matches!(
                self.coordinator.as_ref().map(Coordinator::state),
                None | Some(State::Idle)
            )
    }

    fn displays(&self, cfg: &Config) -> Vec<Arc<dyn DisplayInput>> {
        match &self.factories {
            Some(factories) => (factories.displays)(cfg),
            None => cfg
                .displays
                .iter()
                .map(|display| {
                    // `DdcDisplay::new` reads an empty `edid_uuid` as "the
                    // first external display", which is what a config with a
                    // single monitor and no UUID wants.
                    Arc::new(DdcDisplay::new(
                        Some(display.edid_uuid.clone()),
                        display.name.clone(),
                    )) as Arc<dyn DisplayInput>
                })
                .collect(),
        }
    }

    fn devices(&self, cfg: &Config) -> Vec<Arc<dyn HostSwitchable>> {
        match &self.factories {
            Some(factories) => (factories.devices)(cfg),
            None => cfg
                .devices
                .iter()
                .map(|device| {
                    Arc::new(LogitechHidpp::new(
                        device.id.clone(),
                        device.serial.clone(),
                        device.name.clone(),
                    )) as Arc<dyn HostSwitchable>
                })
                .collect(),
        }
    }

    fn status(&self) -> Status {
        let state = match self.coordinator.as_ref().map(Coordinator::state) {
            None => STATE_UNCONFIGURED,
            Some(State::Idle) => "Idle",
            Some(State::Confirming { .. }) => "Confirming",
            Some(State::Switching { .. }) => "Switching",
            Some(State::Cooldown { .. }) => "Cooldown",
        };
        Status {
            state: state.to_string(),
            // A config kept only to let a running switch finish is not usable.
            config_ok: self.cfg.is_some() && self.config_error.is_none(),
            config_error: self.config_error.clone(),
            this_host: self.cfg.as_ref().map(|cfg| cfg.this_host),
            hosts: self
                .cfg
                .as_ref()
                .map(|cfg| {
                    cfg.hosts
                        .iter()
                        .map(|host| (host.index, host.name.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            input_monitoring: permissions::input_monitoring_granted(),
            last_report: self.last_report.clone(),
            language: self
                .cfg
                .as_ref()
                .map_or(Language::Auto, |cfg| cfg.options.language)
                .resolve(),
        }
    }

    fn publish(&self) {
        self.status_tx.send_replace(self.status());
    }
}

fn load(path: &Path) -> Result<Config, ConfigError> {
    let cfg = Config::load(path)?;
    cfg.validate()?;
    Ok(cfg)
}

/// A one-step report for a switch that could not run, so that the coordinator
/// always sees the `SwitchFinished` it is waiting for.
fn failed_report(target: HostIndex, detail: String) -> SwitchReport {
    SwitchReport {
        target,
        steps: vec![StepResult {
            what: format!("plan for host {target}"),
            ok: false,
            detail,
            ms: 0,
        }],
        ok: false,
    }
}

/// The vendor ids [`Core::start`] built the HID matcher with; empty means it
/// matches every device, which is also what an unreadable config gives.
fn matcher_vendors(path: &Path) -> Vec<u16> {
    let mut vendors: Vec<u16> = Config::load(path)
        .map(|cfg| {
            cfg.devices
                .iter()
                .filter_map(|device| device.id.vid_pid())
                .map(|(vid, _)| vid)
                .collect()
        })
        .unwrap_or_default();
    vendors.sort_unstable();
    vendors.dedup();
    vendors
}

/// `Instant::now` as the coordinator wants it, and as tokio's (test) clock
/// sees it: with a paused clock both must move together.
fn now() -> std::time::Instant {
    Instant::now().into_std()
}

/// A sleep the `select!` can always build; the branch's own precondition
/// decides whether it is used.
async fn sleep_until(deadline: Option<Instant>) {
    tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::Semaphore;

    use crate::config::test_support::{two_host_config, KEYBOARD_ID, MOUSE_ID};
    use crate::device::{DeviceError, HostSwitchable};
    use crate::display::{DisplayError, DisplayInput};
    use crate::types::{DeviceId, HostInfo};

    /// The calls the fakes saw, in order.
    #[derive(Clone, Default)]
    struct CallLog(Arc<Mutex<Vec<String>>>);

    impl CallLog {
        fn record(&self, call: impl Into<String>) {
            self.0.lock().expect("call log").push(call.into());
        }

        fn calls(&self) -> Vec<String> {
            self.0.lock().expect("call log").clone()
        }
    }

    /// What the fake monitors answer when they are asked which input they are
    /// showing, and how often they were asked. Reads are counted rather than
    /// logged in [`CallLog`]: they are not steps of a switch, and every other
    /// test asserts the switch steps verbatim.
    #[derive(Clone, Default)]
    struct FakeScreen {
        /// `None` is a display that will not say — the default everywhere, so
        /// the tests that predate the read keep their old behaviour.
        code: Option<u8>,
        /// How long the fake takes to answer; `None` answers at once.
        delay: Option<Duration>,
        reads: Arc<AtomicUsize>,
    }

    struct FakeDisplay {
        name: String,
        log: CallLog,
        /// When set, the write blocks until the test hands out a permit.
        gate: Option<Arc<Semaphore>>,
        screen: FakeScreen,
    }

    #[async_trait]
    impl DisplayInput for FakeDisplay {
        fn name(&self) -> &str {
            &self.name
        }

        async fn set_input(&self, code: u8) -> Result<(), DisplayError> {
            self.log
                .record(format!("display {} set_input {code}", self.name));
            if let Some(gate) = &self.gate {
                let _permit = gate.acquire().await;
            }
            Ok(())
        }

        async fn current_input(&self) -> Option<u8> {
            self.screen.reads.fetch_add(1, Ordering::SeqCst);
            if let Some(delay) = self.screen.delay {
                tokio::time::sleep(delay).await;
            }
            self.screen.code
        }
    }

    struct FakeDevice {
        id: DeviceId,
        name: String,
        log: CallLog,
        /// When set, the switch panics, standing in for an adapter that dies
        /// and takes its task with it.
        panic_on_switch: bool,
    }

    #[async_trait]
    impl HostSwitchable for FakeDevice {
        fn id(&self) -> &DeviceId {
            &self.id
        }

        fn name(&self) -> &str {
            &self.name
        }

        async fn host_info(&self) -> Result<HostInfo, DeviceError> {
            self.log.record(format!("device {} host_info", self.name));
            Ok(HostInfo {
                count: 3,
                current: 0,
            })
        }

        async fn switch_to_host(&self, index: HostIndex) -> Result<(), DeviceError> {
            self.log
                .record(format!("device {} switch_to_host {index}", self.name));
            assert!(!self.panic_on_switch, "the fake device panics on purpose");
            Ok(())
        }
    }

    fn factories(
        log: &CallLog,
        gate: Option<Arc<Semaphore>>,
        panic_on_switch: bool,
        screen: FakeScreen,
    ) -> Factories {
        let (displays_log, devices_log) = (log.clone(), log.clone());
        Factories {
            displays: Arc::new(move |cfg: &Config| {
                cfg.displays
                    .iter()
                    .map(|display| {
                        Arc::new(FakeDisplay {
                            name: display.name.clone(),
                            log: displays_log.clone(),
                            gate: gate.clone(),
                            screen: screen.clone(),
                        }) as Arc<dyn DisplayInput>
                    })
                    .collect()
            }),
            devices: Arc::new(move |cfg: &Config| {
                cfg.devices
                    .iter()
                    .map(|device| {
                        Arc::new(FakeDevice {
                            id: device.id.clone(),
                            name: device.name.clone(),
                            log: devices_log.clone(),
                            panic_on_switch,
                        }) as Arc<dyn HostSwitchable>
                    })
                    .collect()
            }),
        }
    }

    fn added(node: usize, id: &str) -> RawHidEvent {
        let (vid, pid) = DeviceId(id.to_string()).vid_pid().expect("test id");
        RawHidEvent::Added {
            node,
            vid,
            pid,
            product: "test device".to_string(),
        }
    }

    /// A started core plus the handles a test drives it with.
    struct Harness {
        handle: CoreHandle,
        raw: mpsc::UnboundedSender<RawHidEvent>,
        config_changed: mpsc::UnboundedSender<()>,
        calls: CallLog,
        screen: FakeScreen,
        path: PathBuf,
        _dir: tempfile::TempDir,
    }

    /// Starts a core over a config directory; `config` is written first when given.
    fn start(config: Option<Config>, gate: Option<Arc<Semaphore>>) -> Harness {
        start_with_devices(config, gate, false, FakeScreen::default())
    }

    /// The same, with devices whose switch panics.
    fn start_panicking(config: Config) -> Harness {
        start_with_devices(Some(config), None, true, FakeScreen::default())
    }

    /// The same, with a monitor that answers `code` when asked which input it
    /// is showing.
    fn start_with_screen_at(config: Config, code: u8) -> Harness {
        let screen = FakeScreen {
            code: Some(code),
            delay: None,
            reads: Arc::default(),
        };
        start_with_devices(Some(config), None, false, screen)
    }

    /// The same, with a monitor that takes `delay` to answer.
    fn start_with_slow_screen_at(config: Config, code: u8, delay: Duration) -> Harness {
        let screen = FakeScreen {
            code: Some(code),
            delay: Some(delay),
            reads: Arc::default(),
        };
        start_with_devices(Some(config), None, false, screen)
    }

    fn start_with_devices(
        config: Option<Config>,
        gate: Option<Arc<Semaphore>>,
        panic_on_switch: bool,
        screen: FakeScreen,
    ) -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        if let Some(config) = config {
            config.save_atomic(&path).expect("write the config");
        }

        let calls = CallLog::default();
        let (raw, raw_rx) = mpsc::unbounded_channel();
        let (config_changed, cfg_rx) = mpsc::unbounded_channel();
        let handle = Core::start_with(
            path.clone(),
            LogBuffer::new(50),
            raw_rx,
            cfg_rx,
            Some(factories(&calls, gate, panic_on_switch, screen.clone())),
        );

        Harness {
            handle,
            raw,
            config_changed,
            calls,
            screen,
            path,
            _dir: dir,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_trigger_leaving_switches_once_the_debounce_expires() {
        let h = start(Some(two_host_config()), None);

        // Both devices are here. The keyboard turning up arms a pull debounce
        // of its own; this core has never switched anything and the default
        // fake monitor will not say where the screen is, so it runs out into
        // `Idle` and leaves the stage to the leave below.
        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        h.raw.send(added(2, MOUSE_ID)).expect("send");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(h.handle.status().await.state, "Idle");

        // Then the keyboard's only node goes away.
        h.raw.send(RawHidEvent::Removed { node: 1 }).expect("send");

        assert_eq!(h.handle.status().await.state, "Confirming");

        tokio::time::sleep(Duration::from_millis(900)).await;
        let status = h.handle.status().await;
        assert_eq!(status.state, "Cooldown");
        assert_eq!(
            h.calls.calls(),
            vec![
                "display AOC U2790R3B set_input 18",
                "device MX Master 3 host_info",
                "device MX Master 3 switch_to_host 1",
            ],
            "the display runs before the following mouse; the keyboard already left"
        );
        assert!(status.last_report.expect("a report").ok);

        // Anything that happens during the cooldown is ignored.
        h.raw.send(added(3, KEYBOARD_ID)).expect("send");
        h.raw.send(RawHidEvent::Removed { node: 3 }).expect("send");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(h.handle.status().await.state, "Cooldown");
        assert_eq!(h.calls.calls().len(), 3, "no second switch");

        // Five seconds after the switch finished, the core is idle again.
        tokio::time::sleep(Duration::from_millis(4200)).await;
        assert_eq!(h.handle.status().await.state, "Idle");
        assert_eq!(h.calls.calls().len(), 3);
    }

    /// The bounce-back path end to end: the keyboard failed to reach the other
    /// Mac and re-appeared here while the cooldown was running, so the core
    /// brings the screen home on its own once the cooldown ends.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_coming_back_during_the_cooldown_switches_home() {
        let h = start(Some(two_host_config()), None);

        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        h.raw.send(added(2, MOUSE_ID)).expect("send");
        // The arrivals' own pull debounce runs out first; nothing to pull.
        tokio::time::sleep(Duration::from_millis(900)).await;
        h.raw.send(RawHidEvent::Removed { node: 1 }).expect("send");

        // The debounce expires and the switch away runs.
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(h.handle.status().await.state, "Cooldown");
        let away = h.calls.calls();
        assert_eq!(away.len(), 3, "{away:?}");

        // The keyboard shows up here again: nothing happens yet, the cooldown
        // is a hardware requirement.
        h.raw.send(added(3, KEYBOARD_ID)).expect("send");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(h.handle.status().await.state, "Cooldown");
        assert_eq!(h.calls.calls().len(), 3, "no switch during the cooldown");

        // Past the cooldown the return switch runs by itself.
        tokio::time::sleep(Duration::from_millis(5000)).await;
        let status = h.handle.status().await;
        assert_eq!(status.state, "Cooldown", "the return switch cools down too");
        let report = status.last_report.expect("a report");
        assert_eq!(report.target, 0, "the return targets this Mac");
        assert!(report.ok, "{report:?}");
        assert_eq!(
            h.calls.calls()[3..],
            [
                "display AOC U2790R3B set_input 17",
                // The fake devices report host 0, so the mouse is already where
                // the plan wants it and no ChangeHost follows.
                "device MX Master 3 host_info",
            ],
            "an automatic switch: only following devices, the keyboard stays put"
        );

        // ...and the chain stops there.
        tokio::time::sleep(Duration::from_millis(5100)).await;
        assert_eq!(h.handle.status().await.state, "Idle");
        assert_eq!(h.calls.calls().len(), 5, "no third switch");
    }

    #[tokio::test(start_paused = true)]
    async fn a_trigger_that_comes_back_cancels_the_switch() {
        let h = start(Some(two_host_config()), None);

        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        h.raw.send(RawHidEvent::Removed { node: 1 }).expect("send");
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.raw.send(added(2, KEYBOARD_ID)).expect("send");

        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(h.handle.status().await.state, "Idle");
        assert!(h.calls.calls().is_empty(), "nothing was switched");
    }

    /// The pull decision end to end (spec §5): this core has never switched
    /// anything, so the old `last_target` rule would have sat still. The
    /// monitor says it is showing host 1, so the keyboard turning up here
    /// brings the screen home instead.
    ///
    /// And the question is asked *late*: not when the keyboard arrives — the
    /// monitor is still re-syncing from the input the other Mac just wrote —
    /// but when the debounce expires, once and no more.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_arriving_while_the_screen_is_elsewhere_pulls_it_home() {
        let h = start_with_screen_at(two_host_config(), 18);

        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(h.handle.status().await.state, "Confirming");
        assert_eq!(
            h.screen.reads.load(Ordering::SeqCst),
            0,
            "the arrival itself asks the monitor nothing",
        );
        assert!(h.calls.calls().is_empty(), "nothing has been switched yet");

        // The mouse turning up is not a trigger: nobody asks the monitor.
        h.raw.send(added(2, MOUSE_ID)).expect("send");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(h.screen.reads.load(Ordering::SeqCst), 0);

        // Past the deadline: the monitor is asked exactly once, and its answer
        // starts the pull.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            h.screen.reads.load(Ordering::SeqCst),
            1,
            "one read, taken when the debounce expired",
        );
        let status = h.handle.status().await;
        assert_eq!(status.state, "Cooldown");
        let report = status.last_report.expect("a report");
        assert_eq!(report.target, 0, "the pull targets this Mac");
        assert!(report.ok, "{report:?}");
        assert_eq!(
            h.calls.calls(),
            vec![
                "display AOC U2790R3B set_input 17",
                // An automatic switch leaves the trigger keyboard where it is.
                "device MX Master 3 host_info",
            ],
        );
    }

    /// And the monitor is only asked on that one path: a trigger *leaving*
    /// needs no answer, so the display is left alone.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_leaving_does_not_ask_the_monitor() {
        let h = start_with_screen_at(two_host_config(), 17);

        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        tokio::time::sleep(Duration::from_millis(900)).await;
        // The debounce expired, the monitor said the screen is already here,
        // so the arrival came to nothing.
        assert_eq!(h.handle.status().await.state, "Idle");
        assert_eq!(h.screen.reads.load(Ordering::SeqCst), 1);

        h.raw.send(RawHidEvent::Removed { node: 1 }).expect("send");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(h.handle.status().await.state, "Cooldown");
        assert_eq!(
            h.screen.reads.load(Ordering::SeqCst),
            1,
            "a leave, a timer and a finished switch never touch the monitor",
        );
    }

    /// A monitor that will not answer must not take the core loop with it: the
    /// decision read has a second and a half, and the arrival is then decided by the
    /// fallback rule. This core has never switched anything, so `last_target`
    /// is `None` and nothing is pulled — even though the (eventual) reading
    /// would have said the screen is on the other host.
    #[tokio::test(start_paused = true)]
    async fn a_slow_monitor_does_not_stall_the_decision() {
        let h = start_with_slow_screen_at(two_host_config(), 18, Duration::from_secs(10));

        h.raw.send(added(1, KEYBOARD_ID)).expect("send");

        // The arrival asks nothing; the read starts when the debounce expires.
        tokio::time::sleep(Duration::from_millis(850)).await;
        assert_eq!(h.screen.reads.load(Ordering::SeqCst), 1);

        // The loop is inside that read when this query is queued behind it.
        let started = Instant::now();
        let status = h.handle.status().await;
        let waited = started.elapsed();

        // The bound is the budget plus the slack this test needs, not a round
        // number: raising `DECISION_READ_TIMEOUT` much further must fail here,
        // because everything else the loop serves waits exactly this long.
        assert!(
            waited < Duration::from_millis(1_750),
            "the loop waited {waited:?} on one read; the budget is {DECISION_READ_TIMEOUT:?}",
        );
        assert_eq!(
            status.state, "Idle",
            "the read timed out, so the fallback decided: nothing to pull back from",
        );
        assert_eq!(h.screen.reads.load(Ordering::SeqCst), 1, "one read only");

        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            h.calls.calls().is_empty(),
            "a late answer arrives after the decision and changes nothing",
        );
        assert_eq!(h.handle.status().await.state, "Idle");
    }

    /// The keyboard touched down here and the user pressed Easy-Switch again
    /// before the monitor had answered. That departure must beat the read: it
    /// abandons it, and the screen goes *after* the keyboard instead of being
    /// pulled home and held here.
    ///
    /// Without the race the `DeviceLeft` would queue behind the read, and by
    /// the time it was seen `TimerFired` would already have started the pull —
    /// `Switching` ignores a leave, so this Mac would keep a screen whose
    /// keyboard is on the other one.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_leaving_while_the_monitor_is_asked_still_switches_away() {
        // The monitor answers well inside the budget, and it answers "the
        // screen is elsewhere" — so without the race this arrival really would
        // pull it home. Only who wins decides which way this goes.
        let h = start_with_slow_screen_at(two_host_config(), 18, Duration::from_millis(400));

        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        h.raw.send(added(2, MOUSE_ID)).expect("send");

        // The debounce expires at 800 ms and the read starts; it is still in
        // flight when this wakes.
        tokio::time::sleep(Duration::from_millis(850)).await;
        assert_eq!(h.screen.reads.load(Ordering::SeqCst), 1, "the read started");
        assert!(h.calls.calls().is_empty(), "nothing has been switched yet");

        // Easy-Switch: the keyboard's node goes away mid-read.
        h.raw.send(RawHidEvent::Removed { node: 1 }).expect("send");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            h.handle.status().await.state,
            "Confirming",
            "the leave was handled at once and armed its own debounce",
        );

        // That leave debounce expires and the switch away runs.
        tokio::time::sleep(Duration::from_millis(900)).await;
        let status = h.handle.status().await;
        assert_eq!(status.state, "Cooldown");
        let report = status.last_report.expect("a report");
        assert_eq!(report.target, 1, "the screen went after the keyboard");
        assert!(report.ok, "{report:?}");
        assert_eq!(
            h.calls.calls(),
            vec![
                "display AOC U2790R3B set_input 18",
                "device MX Master 3 host_info",
                "device MX Master 3 switch_to_host 1",
            ],
        );
        assert_eq!(
            h.screen.reads.load(Ordering::SeqCst),
            1,
            "the abandoned read is never retried",
        );

        // The answer the abandoned read would have given is gone for good, and
        // the cooldown ends in `Idle`.
        tokio::time::sleep(Duration::from_millis(5100)).await;
        assert_eq!(h.handle.status().await.state, "Idle");
        assert_eq!(h.calls.calls().len(), 3, "no second switch");
    }

    /// The pull turned off is also a promise not to disturb the monitor.
    #[tokio::test(start_paused = true)]
    async fn an_arrival_asks_the_monitor_nothing_when_the_pull_is_off() {
        let mut cfg = two_host_config();
        cfg.options.pull_on_arrival = false;
        // The screen is on the other host, so only the option stands between
        // this arrival and a pull.
        let h = start_with_screen_at(cfg, 18);

        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        tokio::time::sleep(Duration::from_millis(900)).await;

        assert_eq!(h.handle.status().await.state, "Idle");
        assert_eq!(
            h.screen.reads.load(Ordering::SeqCst),
            0,
            "nobody asked the monitor anything",
        );
        assert!(h.calls.calls().is_empty(), "and nothing was switched");
    }

    #[tokio::test(start_paused = true)]
    async fn a_manual_switch_moves_every_device_and_reports() {
        let h = start(Some(two_host_config()), None);

        let report = h.handle.switch(1, "cli").await.expect("a report");

        assert!(report.ok, "{report:?}");
        assert_eq!(report.target, 1);
        assert_eq!(
            h.calls.calls(),
            vec![
                "display AOC U2790R3B set_input 18",
                "device MX Mechanical host_info",
                "device MX Mechanical switch_to_host 1",
                "device MX Master 3 host_info",
                "device MX Master 3 switch_to_host 1",
            ],
            "a manual switch takes the keyboard along"
        );
        assert_eq!(h.handle.status().await.state, "Cooldown");
    }

    #[tokio::test(start_paused = true)]
    async fn a_switch_while_one_is_running_is_busy() {
        let gate = Arc::new(Semaphore::new(0));
        let h = start(Some(two_host_config()), Some(gate.clone()));

        let handle = h.handle.clone();
        let first = tokio::spawn(async move { handle.switch(1, "cli").await });
        while h.handle.status().await.state != "Switching" {
            tokio::task::yield_now().await;
        }

        let second = h.handle.switch(1, "tray").await;
        assert!(matches!(second, Err(CoreError::Busy)), "{second:?}");

        gate.add_permits(1);
        assert!(first.await.expect("the task").expect("a report").ok);
    }

    #[tokio::test(start_paused = true)]
    async fn a_switch_to_an_undeclared_host_is_a_plan_error() {
        let h = start(Some(two_host_config()), None);

        let result = h.handle.switch(7, "cli").await;

        assert!(
            matches!(
                result,
                Err(CoreError::Plan(PlanError::TargetNotDeclared(7)))
            ),
            "{result:?}"
        );
        assert!(h.calls.calls().is_empty());
        assert_eq!(h.handle.status().await.state, "Idle");
    }

    #[tokio::test(start_paused = true)]
    async fn dry_run_returns_the_manual_plan_without_touching_anything() {
        let h = start(Some(two_host_config()), None);

        let plan = h.handle.dry_run(1).await.expect("a plan");

        assert_eq!(plan.target, 1);
        assert_eq!(plan.displays, vec![("AOC U2790R3B".to_string(), 18)]);
        assert_eq!(plan.devices.len(), 2);
        assert!(h.calls.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_config_everything_is_unconfigured() {
        let h = start(None, None);

        let status = h.handle.status().await;
        assert_eq!(status.state, "Unconfigured");
        assert!(!status.config_ok);
        assert!(status.config_error.is_some());
        assert!(status.hosts.is_empty());
        assert_eq!(status.this_host, None);

        assert!(matches!(
            h.handle.switch(1, "cli").await,
            Err(CoreError::Unconfigured)
        ));
        assert!(matches!(
            h.handle.dry_run(1).await,
            Err(CoreError::Unconfigured)
        ));

        // Triggers are dropped while there is nothing to trigger.
        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        h.raw.send(RawHidEvent::Removed { node: 1 }).expect("send");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(h.calls.calls().is_empty());
        assert_eq!(h.handle.status().await.state, "Unconfigured");
    }

    #[tokio::test(start_paused = true)]
    async fn an_invalid_config_is_reported_and_then_repaired() {
        let h = start(None, None);
        assert!(matches!(h.handle.reload().await, Err(CoreError::Config(_))));

        two_host_config()
            .save_atomic(&h.path)
            .expect("write the config");
        h.handle.reload().await.expect("the new config");

        let status = h.handle.status().await;
        assert_eq!(status.state, "Idle");
        assert!(status.config_ok);
        assert_eq!(status.config_error, None);
        assert_eq!(status.this_host, Some(0));
        assert_eq!(status.hosts.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_config_change_on_disk_is_picked_up() {
        let h = start(Some(two_host_config()), None);

        let mut changed = two_host_config();
        changed.hosts[1].name = "Bam.Studio".to_string();
        changed.save_atomic(&h.path).expect("write the config");
        h.config_changed.send(()).expect("send");

        // The loop needs one turn to reload.
        let status = h.handle.status().await;
        assert_eq!(status.hosts[1].1, "Bam.Studio");
    }

    #[tokio::test(start_paused = true)]
    async fn a_config_change_waits_for_the_switch_to_finish() {
        let gate = Arc::new(Semaphore::new(0));
        let h = start(Some(two_host_config()), Some(gate.clone()));

        let handle = h.handle.clone();
        let switching = tokio::spawn(async move { handle.switch(1, "cli").await });
        while h.handle.status().await.state != "Switching" {
            tokio::task::yield_now().await;
        }

        let mut changed = two_host_config();
        changed.hosts[1].name = "Bam.Studio".to_string();
        changed.save_atomic(&h.path).expect("write the config");
        h.config_changed.send(()).expect("send");

        assert_eq!(
            h.handle.status().await.hosts[1].1,
            "Bam.Mini",
            "the running switch keeps the config it started with"
        );

        gate.add_permits(1);
        switching.await.expect("the task").expect("a report");
        tokio::time::sleep(Duration::from_millis(5100)).await;

        let status = h.handle.status().await;
        assert_eq!(status.state, "Idle");
        assert_eq!(status.hosts[1].1, "Bam.Studio");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_reload_during_a_switch_cannot_let_a_second_one_start() {
        let gate = Arc::new(Semaphore::new(0));
        let h = start(Some(two_host_config()), Some(gate.clone()));

        let handle = h.handle.clone();
        let first = tokio::spawn(async move { handle.switch(1, "cli").await });
        while h.handle.status().await.state != "Switching" {
            tokio::task::yield_now().await;
        }

        // The config goes bad while the switch runs: the coordinator must stay
        // so that the report still lands and the cooldown still happens.
        std::fs::write(&h.path, "{ not json").expect("write the config");
        assert!(matches!(h.handle.reload().await, Err(CoreError::Config(_))));
        let status = h.handle.status().await;
        assert_eq!(status.state, "Switching");
        assert!(!status.config_ok);
        assert!(status.config_error.is_some());

        // ...and good again, which must not hand out a fresh idle coordinator.
        two_host_config()
            .save_atomic(&h.path)
            .expect("write the config");
        h.handle.reload().await.expect("the new config");

        let second = h.handle.switch(1, "tray").await;
        assert!(matches!(second, Err(CoreError::Busy)), "{second:?}");

        gate.add_permits(1);
        assert!(first.await.expect("the task").expect("a report").ok);
        assert_eq!(h.handle.status().await.state, "Cooldown");

        tokio::time::sleep(Duration::from_millis(5100)).await;
        assert_eq!(h.handle.status().await.state, "Idle");
        assert_eq!(
            h.calls.calls(),
            vec![
                "display AOC U2790R3B set_input 18",
                "device MX Mechanical host_info",
                "device MX Mechanical switch_to_host 1",
                "device MX Master 3 host_info",
                "device MX Master 3 switch_to_host 1",
            ],
            "exactly one switch ran"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reload_during_cooldown_cannot_skip_the_cooldown() {
        let h = start(Some(two_host_config()), None);

        // Run a manual switch to completion, landing in `Cooldown`.
        let report = h.handle.switch(1, "cli").await.expect("a report");
        assert!(report.ok, "{report:?}");
        assert_eq!(h.handle.status().await.state, "Cooldown");

        // An invalid config lands while cooling down: it must not drop the
        // coordinator, only record the error.
        std::fs::write(&h.path, "{ not json").expect("write the config");
        assert!(matches!(h.handle.reload().await, Err(CoreError::Config(_))));
        let status = h.handle.status().await;
        assert_eq!(
            status.state, "Cooldown",
            "the coordinator must survive an invalid reload during cooldown"
        );
        assert!(!status.config_ok);
        assert!(status.config_error.is_some());

        // ...and a valid config right after must not install a fresh, idle
        // coordinator: that would let a switch bypass the cooldown entirely.
        let mut changed = two_host_config();
        changed.hosts[1].name = "Bam.Studio".to_string();
        changed.save_atomic(&h.path).expect("write the config");
        h.handle.reload().await.expect("the new config");

        assert_eq!(
            h.handle.status().await.state,
            "Cooldown",
            "a valid reload must not bypass the cooldown either"
        );

        let during_cooldown = h.handle.switch(1, "tray").await;
        assert!(
            matches!(during_cooldown, Err(CoreError::Busy)),
            "{during_cooldown:?}"
        );

        // Once the cooldown actually elapses, the pending config is installed.
        tokio::time::sleep(Duration::from_millis(5100)).await;
        let status = h.handle.status().await;
        assert_eq!(status.state, "Idle");
        assert_eq!(status.hosts[1].1, "Bam.Studio");
    }

    #[tokio::test(start_paused = true)]
    async fn nodes_seen_before_the_config_still_trigger_after_a_reload() {
        let h = start(None, None);

        // Both devices are already on the bus while nothing is watched yet.
        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        h.raw.send(added(2, MOUSE_ID)).expect("send");

        two_host_config()
            .save_atomic(&h.path)
            .expect("write the config");
        h.handle.reload().await.expect("the new config");

        h.raw.send(RawHidEvent::Removed { node: 1 }).expect("send");
        tokio::time::sleep(Duration::from_millis(900)).await;

        assert_eq!(h.handle.status().await.state, "Cooldown");
        assert_eq!(
            h.calls.calls(),
            vec![
                "display AOC U2790R3B set_input 18",
                "device MX Master 3 host_info",
                "device MX Master 3 switch_to_host 1",
            ],
            "the keyboard left even though it arrived before the config"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_switch_task_that_panics_still_reports_and_cools_down() {
        let h = start_panicking(two_host_config());

        let report = h.handle.switch(1, "cli").await.expect("a report");

        assert!(!report.ok, "{report:?}");
        assert_eq!(report.target, 1);
        assert_eq!(h.handle.status().await.state, "Cooldown");
    }

    #[tokio::test(start_paused = true)]
    async fn the_status_channel_sees_every_transition() {
        let h = start(Some(two_host_config()), None);
        let mut updates = h.handle.subscribe();

        // The first status the core published while starting up.
        updates.changed().await.expect("an update");
        assert_eq!(updates.borrow_and_update().state, "Idle");

        // The arrival arms a pull debounce; with nothing to pull it runs out
        // into `Idle` again, and the tray sees both steps.
        h.raw.send(added(1, KEYBOARD_ID)).expect("send");
        updates.changed().await.expect("an update");
        assert_eq!(updates.borrow_and_update().state, "Confirming");
        tokio::time::sleep(Duration::from_millis(900)).await;
        updates.changed().await.expect("an update");
        assert_eq!(updates.borrow_and_update().state, "Idle");

        h.raw.send(RawHidEvent::Removed { node: 1 }).expect("send");
        updates.changed().await.expect("an update");
        assert_eq!(updates.borrow_and_update().state, "Confirming");

        // A plain status query must not wake the tray again.
        h.handle.status().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), updates.changed())
                .await
                .is_err(),
            "reading the status is not a change"
        );
    }
}
