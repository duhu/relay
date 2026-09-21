//! Running a [`SwitchPlan`] against the device and display layers.
//!
//! Order is fixed by invariant 2 in `docs/overview.md`: displays first, then
//! devices. Nothing here decides *whether* to switch — that is the
//! coordinator's job — and no step aborts the rest of the plan: a screen that
//! refuses to change input must not strand the keyboard on this machine.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::device::{DeviceError, HostSwitchable};
use crate::display::{DisplayError, DisplayInput};
use crate::plan::SwitchPlan;
use crate::types::HostIndex;

/// Backoff before the second, third and every later DDC attempt (spec §3).
const DDC_BACKOFF: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// One thing the plan asked for and how it went.
#[derive(Clone, Debug, serde::Serialize)]
pub struct StepResult {
    pub what: String,
    pub ok: bool,
    pub detail: String,
    pub ms: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SwitchReport {
    pub target: HostIndex,
    pub steps: Vec<StepResult>,
    /// True when every step succeeded.
    pub ok: bool,
}

/// Executes `plan`, logging every step.
///
/// `displays` and `devices` are everything the core currently has; the plan
/// picks from them by display name and by device id. Anything the plan names
/// but that is missing here becomes a failed step, and the run carries on: one
/// dead screen must not leave the keyboard behind on this machine.
pub async fn run_plan(
    plan: &SwitchPlan,
    displays: &[Arc<dyn DisplayInput>],
    devices: &[Arc<dyn HostSwitchable>],
    ddc_retries: u32,
) -> SwitchReport {
    let mut steps = Vec::with_capacity(plan.displays.len() + plan.devices.len());

    for (name, input) in &plan.displays {
        let what = format!("display {name}");
        let started = Instant::now();
        let outcome = match displays.iter().find(|display| display.name() == name) {
            Some(display) => set_input_with_retries(display.as_ref(), *input, ddc_retries)
                .await
                .map(|()| format!("input {input}"))
                .map_err(|err| err.to_string()),
            None => Err("display not found".to_string()),
        };
        steps.push(finish(what, started, outcome));
    }

    for id in &plan.devices {
        let started = Instant::now();
        let Some(device) = devices.iter().find(|device| device.id() == id) else {
            steps.push(finish(
                format!("device {id}"),
                started,
                Err("device not found".to_string()),
            ));
            continue;
        };

        let what = format!("device {}", device.name());
        let outcome = match device.host_info().await {
            // It is already where we want it: another host may have moved it,
            // or the user pressed Easy-Switch by hand.
            Ok(info) if info.current == plan.target => Ok("already at target".to_string()),
            // The other host already took it: on a switch this machine started
            // by itself that is the normal outcome, not a failure.
            Err(DeviceError::NotFound) if !plan.manual => {
                Ok("not on this Mac, skipped".to_string())
            }
            // Without a reading we do not know the device is here and paired,
            // so we do not fire a ChangeHost blind (see `AGENTS.md`).
            Err(err) => Err(err.to_string()),
            // The device knows how many Easy-Switch slots it has; a target past
            // the last one is undeclared and may be unpaired, so never send it
            // (see `AGENTS.md`).
            Ok(info) if plan.target >= info.count => Err(format!(
                "target {} is beyond the device's {} hosts",
                plan.target, info.count
            )),
            Ok(_) => device
                .switch_to_host(plan.target)
                .await
                .map(|()| format!("switched to host {}", plan.target))
                .map_err(|err| err.to_string()),
        };
        steps.push(finish(what, started, outcome));
    }

    let ok = steps.iter().all(|step| step.ok);
    SwitchReport {
        target: plan.target,
        steps,
        ok,
    }
}

/// Writes one DDC input, retrying on failure.
///
/// Total attempts are `ddc_retries + 1` (spec §3: the initial attempt plus
/// the configured retries) — `ddc_retries = 3` means four attempts — with
/// [`DDC_BACKOFF`] waited before each attempt after the first; an attempt
/// past the table reuses its last entry.
async fn set_input_with_retries(
    display: &dyn DisplayInput,
    code: u8,
    ddc_retries: u32,
) -> Result<(), DisplayError> {
    let attempts = ddc_retries.saturating_add(1);
    // Bound outside the loop: `display` is also a `tracing` field helper.
    let name = display.name().to_string();
    let mut last_error = None;

    for attempt in 0..attempts {
        if attempt > 0 {
            let backoff = DDC_BACKOFF[(attempt as usize - 1).min(DDC_BACKOFF.len() - 1)];
            tokio::time::sleep(backoff).await;
        }
        match display.set_input(code).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                tracing::warn!(
                    display = name,
                    attempt = attempt + 1,
                    of = attempts,
                    error = %err,
                    "setting the display input failed"
                );
                last_error = Some(err);
            }
        }
    }

    Err(last_error.expect("at least one attempt has run"))
}

/// Turns one step's outcome into a [`StepResult`] and logs it.
fn finish(what: String, started: Instant, outcome: Result<String, String>) -> StepResult {
    let (ok, detail) = match outcome {
        Ok(detail) => (true, detail),
        Err(detail) => (false, detail),
    };
    let step = StepResult {
        what,
        ok,
        detail,
        ms: started.elapsed().as_millis() as u64,
    };
    tracing::info!(
        what = step.what,
        ok = step.ok,
        detail = step.detail,
        ms = step.ms,
        "switch step"
    );
    step
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use crate::device::DeviceError;
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

    /// More failures than any test lets the executor attempt.
    const ALWAYS: usize = 10;

    struct FakeDisplay {
        name: String,
        log: CallLog,
        /// Popped per call; an empty queue means success.
        results: Mutex<VecDeque<Result<(), DisplayError>>>,
    }

    impl FakeDisplay {
        /// A display that always accepts the write.
        fn working(name: &str, log: &CallLog) -> Arc<dyn DisplayInput> {
            Self::failing(name, log, 0)
        }

        /// Fails `failures` times before succeeding.
        fn failing(name: &str, log: &CallLog, failures: usize) -> Arc<dyn DisplayInput> {
            let results = (0..failures)
                .map(|_| Err(DisplayError::Tool("no response from display".to_string())))
                .collect();
            Arc::new(FakeDisplay {
                name: name.to_string(),
                log: log.clone(),
                results: Mutex::new(results),
            })
        }
    }

    #[async_trait]
    impl DisplayInput for FakeDisplay {
        fn name(&self) -> &str {
            &self.name
        }

        async fn set_input(&self, code: u8) -> Result<(), DisplayError> {
            self.log
                .record(format!("display {} set_input {code}", self.name));
            self.results
                .lock()
                .expect("results")
                .pop_front()
                .unwrap_or(Ok(()))
        }
    }

    struct FakeDevice {
        id: DeviceId,
        name: String,
        log: CallLog,
        host_info: Mutex<VecDeque<Result<HostInfo, DeviceError>>>,
        switches: Mutex<VecDeque<Result<(), DeviceError>>>,
    }

    impl FakeDevice {
        fn with_host_info(
            name: &str,
            log: &CallLog,
            host_info: Result<HostInfo, DeviceError>,
        ) -> Arc<dyn HostSwitchable> {
            Arc::new(FakeDevice {
                id: DeviceId(name.to_string()),
                name: name.to_string(),
                log: log.clone(),
                host_info: Mutex::new(VecDeque::from([host_info])),
                switches: Mutex::new(VecDeque::new()),
            })
        }

        /// A device sitting on host `current`, whose switches succeed.
        fn at_host(name: &str, log: &CallLog, current: u8) -> Arc<dyn HostSwitchable> {
            Self::with_host_info(name, log, Ok(HostInfo { count: 3, current }))
        }
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
            self.host_info
                .lock()
                .expect("host info")
                .pop_front()
                .unwrap_or(Ok(HostInfo {
                    count: 3,
                    current: 0,
                }))
        }

        async fn switch_to_host(&self, index: HostIndex) -> Result<(), DeviceError> {
            self.log
                .record(format!("device {} switch_to_host {index}", self.name));
            self.switches
                .lock()
                .expect("switches")
                .pop_front()
                .unwrap_or(Ok(()))
        }
    }

    /// An automatic plan, as a leaving trigger device produces.
    fn plan() -> SwitchPlan {
        SwitchPlan {
            target: 1,
            displays: vec![("AOC U2790R3B".to_string(), 18)],
            devices: vec![DeviceId("MX Master 3".to_string())],
            manual: false,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn displays_run_before_devices() {
        let log = CallLog::default();
        let displays = [FakeDisplay::working("AOC U2790R3B", &log)];
        let devices = [FakeDevice::at_host("MX Master 3", &log, 0)];

        let report = run_plan(&plan(), &displays, &devices, 3).await;

        assert!(report.ok, "{report:?}");
        assert_eq!(
            log.calls(),
            vec![
                "display AOC U2790R3B set_input 18",
                "device MX Master 3 host_info",
                "device MX Master 3 switch_to_host 1",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_already_at_the_target_is_skipped() {
        let log = CallLog::default();
        let displays = [FakeDisplay::working("AOC U2790R3B", &log)];
        let devices = [FakeDevice::at_host("MX Master 3", &log, 1)];

        let report = run_plan(&plan(), &displays, &devices, 3).await;

        assert!(report.ok, "{report:?}");
        assert_eq!(
            log.calls(),
            vec![
                "display AOC U2790R3B set_input 18",
                "device MX Master 3 host_info",
            ]
        );
        assert_eq!(report.steps[1].detail, "already at target");
    }

    #[tokio::test(start_paused = true)]
    async fn a_target_beyond_the_devices_host_count_is_never_sent() {
        let log = CallLog::default();
        let displays = [FakeDisplay::working("AOC U2790R3B", &log)];
        // Two slots means the valid targets are 0 and 1; slot 2 is past the end
        // of this device's Easy-Switch slots and may well be unpaired.
        let devices = [FakeDevice::with_host_info(
            "MX Master 3",
            &log,
            Ok(HostInfo {
                count: 2,
                current: 0,
            }),
        )];
        let plan = SwitchPlan {
            target: 2,
            ..plan()
        };

        let report = run_plan(&plan, &displays, &devices, 3).await;

        assert!(!report.ok, "an out-of-range target fails the run");
        assert!(!report.steps[1].ok);
        assert!(
            report.steps[1].detail.contains("beyond"),
            "detail was {:?}",
            report.steps[1].detail
        );
        assert_eq!(
            log.calls(),
            vec![
                "display AOC U2790R3B set_input 18",
                "device MX Master 3 host_info",
            ],
            "switch_to_host must never be called"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_display_does_not_stop_the_devices() {
        let log = CallLog::default();
        let displays = [FakeDisplay::failing("AOC U2790R3B", &log, ALWAYS)];
        let devices = [FakeDevice::at_host("MX Master 3", &log, 0)];

        let report = run_plan(&plan(), &displays, &devices, 1).await;

        assert!(!report.ok, "a failed display makes the report not ok");
        assert!(!report.steps[0].ok);
        assert!(report.steps[1].ok, "the device still switched");
        assert_eq!(
            log.calls().last().map(String::as_str),
            Some("device MX Master 3 switch_to_host 1")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn set_input_is_attempted_ddc_retries_plus_one_times() {
        let log = CallLog::default();
        let displays = [FakeDisplay::failing("AOC U2790R3B", &log, ALWAYS)];
        let devices: [Arc<dyn HostSwitchable>; 0] = [];
        let plan = SwitchPlan {
            devices: vec![],
            ..plan()
        };

        let started = Instant::now();
        let report = run_plan(&plan, &displays, &devices, 3).await;
        let elapsed = started.elapsed();

        assert_eq!(
            log.calls().len(),
            4,
            "one initial attempt plus three retries: {:?}",
            log.calls()
        );
        assert!(!report.steps[0].ok);
        assert_eq!(
            elapsed,
            Duration::from_millis(3500),
            "backoffs of 0.5s, 1s and 2s waited between the four attempts"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn set_input_stops_retrying_once_it_succeeds() {
        let log = CallLog::default();
        let displays = [FakeDisplay::failing("AOC U2790R3B", &log, 1)];
        let devices: [Arc<dyn HostSwitchable>; 0] = [];
        let plan = SwitchPlan {
            devices: vec![],
            ..plan()
        };

        let report = run_plan(&plan, &displays, &devices, 3).await;

        assert_eq!(log.calls().len(), 2, "{:?}", log.calls());
        assert!(report.ok, "{report:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_display_the_core_does_not_know_is_a_failed_step() {
        let log = CallLog::default();
        let displays: [Arc<dyn DisplayInput>; 0] = [];
        let devices = [FakeDevice::at_host("MX Master 3", &log, 0)];

        let report = run_plan(&plan(), &displays, &devices, 3).await;

        assert!(!report.ok);
        assert_eq!(report.steps[0].detail, "display not found");
        assert!(report.steps[1].ok, "the device still switched");
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_that_left_the_machine_fails_a_manual_switch_and_the_next_one_still_switches()
    {
        let log = CallLog::default();
        let displays = [FakeDisplay::working("AOC U2790R3B", &log)];
        let devices = [
            FakeDevice::with_host_info("MX Master 3", &log, Err(DeviceError::NotFound)),
            FakeDevice::at_host("MX Mechanical", &log, 0),
        ];
        let plan = SwitchPlan {
            devices: vec![
                DeviceId("MX Master 3".to_string()),
                DeviceId("MX Mechanical".to_string()),
            ],
            manual: true,
            ..plan()
        };

        let report = run_plan(&plan, &displays, &devices, 3).await;

        assert!(!report.ok);
        assert!(!report.steps[1].ok, "the missing device failed");
        assert_eq!(report.steps[1].detail, "device not present");
        assert!(report.steps[2].ok, "the next device still switched");
        assert_eq!(
            log.calls().last().map(String::as_str),
            Some("device MX Mechanical switch_to_host 1")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_automatic_switch_skips_a_device_the_other_host_already_took() {
        let log = CallLog::default();
        let displays = [FakeDisplay::working("AOC U2790R3B", &log)];
        let devices = [
            FakeDevice::with_host_info("MX Master 3", &log, Err(DeviceError::NotFound)),
            FakeDevice::at_host("MX Mechanical", &log, 0),
        ];
        let plan = SwitchPlan {
            devices: vec![
                DeviceId("MX Master 3".to_string()),
                DeviceId("MX Mechanical".to_string()),
            ],
            ..plan()
        };

        let report = run_plan(&plan, &displays, &devices, 3).await;

        assert!(report.ok, "a device already moved away is not a failure");
        assert!(report.steps[1].ok, "{:?}", report.steps[1]);
        assert!(
            report.steps[1].detail.contains("skipped"),
            "detail was {:?}",
            report.steps[1].detail
        );
        assert!(report.steps[2].ok, "the next device still switched");
        assert_eq!(
            log.calls().last().map(String::as_str),
            Some("device MX Mechanical switch_to_host 1"),
            "the skipped device is never sent a ChangeHost"
        );
    }
}
