//! Turning "switch to host N" into the concrete list of things to do.

use crate::config::Config;
use crate::types::{DeviceId, HostIndex};

/// Everything one switch touches, in execution order: displays first, then devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchPlan {
    pub target: HostIndex,
    /// Display name and the DDC input source to select on it.
    pub displays: Vec<(String, u8)>,
    pub devices: Vec<DeviceId>,
    /// True when a person asked for this switch (CLI, tray, hotkey); false for
    /// a switch the coordinator started on its own. The executor treats a
    /// device another host already took as a skip only on an automatic switch.
    pub manual: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("host {0} is not declared in config")]
    TargetNotDeclared(HostIndex),
    #[error("display '{display}' has no input for host {host}")]
    MissingInput { display: String, host: HostIndex },
}

/// Builds the plan for switching to `target`.
///
/// An automatic switch moves only the `follow` devices (the trigger keyboard
/// already left on its own); a manual one moves every configured device.
pub fn build_plan(cfg: &Config, target: HostIndex, manual: bool) -> Result<SwitchPlan, PlanError> {
    if !cfg.has_host(target) {
        return Err(PlanError::TargetNotDeclared(target));
    }

    let mut displays = Vec::with_capacity(cfg.displays.len());
    for display in &cfg.displays {
        let input = display
            .input_for(target)
            .ok_or_else(|| PlanError::MissingInput {
                display: display.name.clone(),
                host: target,
            })?;
        displays.push((display.name.clone(), input));
    }

    let devices = cfg
        .devices
        .iter()
        .filter(|d| manual || d.follow)
        .map(|d| d.id.clone())
        .collect();

    Ok(SwitchPlan {
        target,
        displays,
        devices,
        manual,
    })
}

/// Where to switch when `trigger` leaves this machine.
///
/// With two hosts the target is the other one; beyond that it must be spelled
/// out as the device's `leave_to` (config validation enforces this).
pub fn target_for_leave(cfg: &Config, trigger: &DeviceId) -> Option<HostIndex> {
    let mut others = cfg
        .hosts
        .iter()
        .map(|h| h.index)
        .filter(|index| *index != cfg.this_host);
    let only_other = others.next().filter(|_| others.next().is_none());
    if let Some(index) = only_other {
        return Some(index);
    }

    let leave_to = cfg.device(trigger)?.leave_to?;
    (leave_to != cfg.this_host && cfg.has_host(leave_to)).then_some(leave_to)
}

/// The host after this one in the configured order, wrapping around.
pub fn next_host(cfg: &Config) -> HostIndex {
    let Some(position) = cfg.hosts.iter().position(|h| h.index == cfg.this_host) else {
        // An invalid config; callers validate first, so just stay put.
        return cfg.this_host;
    };
    cfg.hosts[(position + 1) % cfg.hosts.len()].index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{three_host_config, two_host_config, KEYBOARD_ID, MOUSE_ID};

    #[test]
    fn build_plan_rejects_an_undeclared_target() {
        let cfg = two_host_config();
        assert!(matches!(
            build_plan(&cfg, 7, false),
            Err(PlanError::TargetNotDeclared(7))
        ));
    }

    #[test]
    fn automatic_plan_carries_displays_and_follow_devices_only() {
        let cfg = two_host_config();
        let plan = build_plan(&cfg, 1, false).expect("plan");
        assert_eq!(plan.target, 1);
        assert_eq!(plan.displays, vec![("AOC U2790R3B".to_string(), 18)]);
        assert_eq!(plan.devices, vec![DeviceId(MOUSE_ID.to_string())]);
    }

    #[test]
    fn manual_plan_includes_the_keyboard() {
        let cfg = two_host_config();
        let plan = build_plan(&cfg, 1, true).expect("plan");
        assert_eq!(
            plan.devices,
            vec![
                DeviceId(KEYBOARD_ID.to_string()),
                DeviceId(MOUSE_ID.to_string())
            ]
        );
    }

    #[test]
    fn build_plan_reports_a_display_without_an_input_for_the_target() {
        let mut cfg = two_host_config();
        cfg.displays[0].input_by_host.remove("1");
        match build_plan(&cfg, 1, false) {
            Err(PlanError::MissingInput { display, host }) => {
                assert_eq!(display, "AOC U2790R3B");
                assert_eq!(host, 1);
            }
            other => panic!("expected MissingInput, got {other:?}"),
        }
    }

    #[test]
    fn two_hosts_infer_the_leave_target() {
        let cfg = two_host_config();
        assert_eq!(
            target_for_leave(&cfg, &DeviceId(KEYBOARD_ID.to_string())),
            Some(1)
        );
    }

    #[test]
    fn three_hosts_use_leave_to() {
        let cfg = three_host_config();
        assert_eq!(
            target_for_leave(&cfg, &DeviceId(KEYBOARD_ID.to_string())),
            Some(2)
        );
    }

    #[test]
    fn three_hosts_without_leave_to_are_ambiguous() {
        let mut cfg = three_host_config();
        cfg.devices[0].leave_to = None;
        assert_eq!(
            target_for_leave(&cfg, &DeviceId(KEYBOARD_ID.to_string())),
            None
        );
    }

    #[test]
    fn leave_target_of_an_unknown_device_is_none() {
        let cfg = three_host_config();
        assert_eq!(
            target_for_leave(&cfg, &DeviceId("dead:beef".to_string())),
            None
        );
    }

    #[test]
    fn next_host_cycles_through_the_host_list() {
        let mut cfg = three_host_config();
        assert_eq!(next_host(&cfg), 1);
        cfg.this_host = 2;
        assert_eq!(next_host(&cfg), 0);
    }
}
