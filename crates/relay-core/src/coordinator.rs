//! The switch state machine.
//!
//! Synchronous and IO-free: the caller feeds it events with the current
//! `Instant` and performs the single [`Action`] it returns. Every hardware
//! action goes through here, so the debounce and the cooldown cannot be
//! bypassed (see `AGENTS.md`, hardware constraints).

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::plan;
use crate::types::{DeviceId, HostIndex, TriggerEvent};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    /// A trigger device left (or arrived); waiting to see whether it changes
    /// its mind straight away.
    Confirming {
        target: HostIndex,
        trigger: DeviceId,
        deadline: Instant,
        /// Which direction started this debounce. `false`: the trigger left,
        /// so the switch hands this machine over to `target` and the same
        /// device *arriving* cancels it — it never really left. `true`: the
        /// trigger arrived and `target` is `this_host`, so the switch pulls
        /// the screen home, and the same device *leaving* drops the pull. The
        /// two are not quite mirror images: a device that leaves has left, so
        /// that case does not simply go idle but starts the leave sequence
        /// afresh (see [`Coordinator::start_leave`]). The rest of the debounce
        /// is identical, except that the arrival direction still has to pass
        /// [`Coordinator::pull_verdict`] when the timer fires, and may end in
        /// `Idle` after all.
        from_arrival: bool,
    },
    Switching {
        target: HostIndex,
        /// The trigger device that bounced back to this machine while the
        /// switch to `target` was running; see [`State::Cooldown::returning`].
        returning: Option<DeviceId>,
    },
    /// Settling after a switch; every trigger is ignored until `until`.
    Cooldown {
        until: Instant,
        /// The switch that just finished handed this machine over to another
        /// host. A switch that targeted `this_host` has nothing to come back
        /// from, so it never arms `returning`.
        away: bool,
        /// The trigger device that came back to this machine during the switch
        /// or the cooldown, so the screen and the following devices belong
        /// here again: when the cooldown ends we switch back to `this_host`
        /// (unless `options.switch_back_on_reconnect` is off).
        ///
        /// The pending return is keyed on the device that armed it, not on
        /// "some trigger device": any other trigger device may report a leave
        /// during the cooldown — a second keyboard going to sleep, or (in a
        /// config predating the rule that `validate` now enforces) a device
        /// that both triggers and follows, losing its HID nodes as part of the
        /// very switch this machine just ran. Validation forbids that pairing
        /// today; the keying stays because the return still belongs to one
        /// device. Another device's `DeviceLeft` must not cancel the return
        /// the keyboard's bounce just armed.
        returning: Option<DeviceId>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Trigger(TriggerEvent),
    /// The timer armed by the last [`Action::ArmTimer`] elapsed.
    TimerFired,
    /// The switch started by [`Action::StartSwitch`] ran to completion.
    SwitchFinished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    ArmTimer(Instant),
    CancelTimer,
    StartSwitch {
        target: HostIndex,
        manual: bool,
    },
    /// Nothing to do; the payload says why, for the log.
    Ignore(&'static str),
}

pub struct Coordinator {
    state: State,
    cfg: Config,
    /// Where the last finished switch sent the screen and the following
    /// devices, which is the only thing this machine knows about where they
    /// are. `None` means this coordinator has not run a switch yet — it was
    /// just started, or rebuilt after a config reload — and the pull-on-arrival
    /// rule stays out of it (spec §6). Deliberately not persisted: a stale
    /// answer from the last run would be worse than no answer.
    last_target: Option<HostIndex>,
    /// What the runtime saw when it last asked the monitor where the screen
    /// is, set through [`Coordinator::observe_screen`] just before the arrival
    /// debounce is allowed to expire. `Some(true)`: the display is showing
    /// this Mac's input. `None` — never asked, or the display would not
    /// answer — falls back to `last_target`. [`Coordinator::pull_verdict`]
    /// takes it, so one reading decides one pull and can never go stale.
    screen_here: Option<bool>,
}

impl Coordinator {
    /// The config must already have passed [`Config::validate`].
    pub fn new(cfg: Config) -> Self {
        Coordinator {
            state: State::Idle,
            cfg,
            last_target: None,
            screen_here: None,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Records what the monitor just said about where the screen is; the
    /// runtime does the reading, this machine stays IO-free.
    ///
    /// `Some(true)` means the display reported the input source this host is
    /// mapped to, `Some(false)` another one, and `None` "we could not tell" —
    /// which is not an error and never changes what the pull would have done
    /// without an answer. The next arrival debounce to expire consumes it.
    pub fn observe_screen(&mut self, here: Option<bool>) {
        self.screen_here = here;
    }

    /// Applies one event and returns the one action the caller must perform.
    pub fn handle(&mut self, ev: Event, now: Instant) -> Action {
        let state = std::mem::replace(&mut self.state, State::Idle);
        let (next, action) = match (state, ev) {
            (State::Idle, Event::Trigger(TriggerEvent::DeviceLeft(id))) => {
                match self.cfg.device(&id) {
                    Some(device) if device.is_trigger => self
                        .start_leave(id, now)
                        .unwrap_or((State::Idle, Action::Ignore("no leave target for trigger"))),
                    _ => (State::Idle, Action::Ignore("not a trigger device")),
                }
            }
            // The mirror of the rule above. The trigger device turned up here
            // while this machine's last switch had handed everything to
            // another host: the user pressed Easy-Switch and the other Mac
            // either is not running Relay or never saw the keyboard leave (it
            // may not have had it in the first place, as after a reboot). So
            // this side may have to pull the screen and the following devices
            // home — whether it really does is settled when the debounce
            // expires, in [`Coordinator::pull_verdict`].
            (State::Idle, Event::Trigger(TriggerEvent::DeviceArrived(id))) => {
                match self.pull_allowed(&id) {
                    Ok(()) => {
                        let deadline = now + self.debounce();
                        (
                            State::Confirming {
                                target: self.cfg.this_host,
                                trigger: id,
                                deadline,
                                from_arrival: true,
                            },
                            Action::ArmTimer(deadline),
                        )
                    }
                    Err(reason) => (State::Idle, Action::Ignore(reason)),
                }
            }
            (State::Idle, Event::Trigger(TriggerEvent::Manual { target, .. })) => {
                // Never start a switch to a slot that may be unpaired.
                if self.cfg.has_host(target) {
                    (
                        State::Switching {
                            target,
                            returning: None,
                        },
                        Action::StartSwitch {
                            target,
                            manual: true,
                        },
                    )
                } else {
                    (State::Idle, Action::Ignore("target not declared"))
                }
            }
            (
                State::Confirming {
                    target,
                    trigger,
                    deadline,
                    from_arrival,
                },
                Event::Trigger(TriggerEvent::DeviceArrived(id)),
            ) => {
                if id == trigger && !from_arrival {
                    // It came back within the debounce: it never really left.
                    (State::Idle, Action::CancelTimer)
                } else {
                    let reason = if id == trigger {
                        // The pull it already armed is still waiting; a second
                        // arrival notification changes nothing.
                        "waiting out the debounce"
                    } else {
                        "a different device arrived"
                    };
                    (
                        State::Confirming {
                            target,
                            trigger,
                            deadline,
                            from_arrival,
                        },
                        Action::Ignore(reason),
                    )
                }
            }
            (
                State::Confirming {
                    target,
                    trigger,
                    deadline,
                    from_arrival,
                },
                Event::Trigger(TriggerEvent::DeviceLeft(id)),
            ) => {
                if id == trigger && from_arrival {
                    // It left again within the debounce: it never really
                    // arrived, so there is nothing to pull home. But this is a
                    // real departure all the same — the keyboard touched down
                    // here and the user pressed Easy-Switch straight on to
                    // another host — so the screen and the following devices
                    // have to go after it rather than stay behind. Start the
                    // leave sequence exactly as `Idle` would; only when the
                    // device has nowhere to leave to is there nothing left but
                    // to drop the pull.
                    self.start_leave(id, now)
                        .unwrap_or((State::Idle, Action::CancelTimer))
                } else {
                    (
                        State::Confirming {
                            target,
                            trigger,
                            deadline,
                            from_arrival,
                        },
                        Action::Ignore("waiting out the debounce"),
                    )
                }
            }
            (
                State::Confirming {
                    target,
                    trigger,
                    deadline,
                    from_arrival,
                },
                Event::TimerFired,
            ) => {
                if now >= deadline {
                    // The pull asks the monitor here rather than back when the
                    // device arrived: right after the other Mac changed the
                    // input the display is still re-syncing, and a read taken
                    // then reports the old input or nothing at all. By the end
                    // of the debounce it has settled, and the answer is also
                    // the freshest one we can get.
                    let verdict = if from_arrival {
                        self.pull_verdict()
                    } else {
                        Ok(())
                    };
                    match verdict {
                        Ok(()) => (
                            State::Switching {
                                target,
                                returning: None,
                            },
                            Action::StartSwitch {
                                target,
                                manual: false,
                            },
                        ),
                        Err(reason) => (State::Idle, Action::Ignore(reason)),
                    }
                } else {
                    // A stale timer (or a clock that ran backwards): the real
                    // deadline has not arrived yet, so keep waiting.
                    (
                        State::Confirming {
                            target,
                            trigger,
                            deadline,
                            from_arrival,
                        },
                        Action::Ignore("timer fired before deadline"),
                    )
                }
            }
            // The trigger device bounced back while this machine was handing
            // everything over. The switch itself runs to the end — cutting it
            // short would leave the screen and the mouse in different places —
            // but the cooldown remembers to bring them home afterwards.
            (
                State::Switching { target, returning },
                Event::Trigger(TriggerEvent::DeviceArrived(id)),
            ) => {
                let returning = self.arm_return(returning, id, true);
                (
                    State::Switching { target, returning },
                    Action::Ignore("a switch is in progress"),
                )
            }
            // It left again before the switch finished: it really is on the
            // other machine, so there is nothing to come back for. Only the
            // device that armed the return can take it away again.
            (
                State::Switching { target, returning },
                Event::Trigger(TriggerEvent::DeviceLeft(id)),
            ) => {
                let returning = clear_return(returning, &id);
                (
                    State::Switching { target, returning },
                    Action::Ignore("a switch is in progress"),
                )
            }
            (State::Switching { target, returning }, Event::SwitchFinished) => {
                let until = now + self.cooldown();
                // Where everything went. Only a finished switch counts: one
                // that is still running has not moved the screen yet.
                self.last_target = Some(target);
                // Only a switch that handed this machine over to another host
                // can be undone by a bounce; a switch to this host is already
                // home.
                let away = target != self.cfg.this_host;
                (
                    State::Cooldown {
                        until,
                        away,
                        returning: if away { returning } else { None },
                    },
                    Action::ArmTimer(until),
                )
            }
            (
                State::Cooldown {
                    until,
                    away,
                    returning,
                },
                Event::Trigger(TriggerEvent::DeviceArrived(id)),
            ) => (
                State::Cooldown {
                    until,
                    away,
                    returning: self.arm_return(returning, id, away),
                },
                Action::Ignore("cooling down after a switch"),
            ),
            (
                State::Cooldown {
                    until,
                    away,
                    returning,
                },
                Event::Trigger(TriggerEvent::DeviceLeft(id)),
            ) => (
                State::Cooldown {
                    until,
                    away,
                    returning: clear_return(returning, &id),
                },
                Action::Ignore("cooling down after a switch"),
            ),
            (
                State::Cooldown {
                    until,
                    away,
                    returning,
                },
                Event::TimerFired,
            ) => {
                if now >= until {
                    // The cooldown is over. Never earlier than this: the wait
                    // between two ChangeHost calls is a hardware requirement
                    // (see `AGENTS.md`).
                    if returning.is_some() && self.cfg.options.switch_back_on_reconnect {
                        let target = self.cfg.this_host;
                        (
                            State::Switching {
                                target,
                                returning: None,
                            },
                            Action::StartSwitch {
                                target,
                                manual: false,
                            },
                        )
                    } else {
                        // The timer that fired needs no further care.
                        (State::Idle, Action::Ignore("cooldown finished"))
                    }
                } else {
                    // A stale timer: the cooldown has not actually elapsed yet.
                    (
                        State::Cooldown {
                            until,
                            away,
                            returning,
                        },
                        Action::Ignore("timer fired before cooldown end"),
                    )
                }
            }
            (state, _) => {
                let reason = ignored_because(&state);
                (state, Action::Ignore(reason))
            }
        };

        self.state = next;
        action
    }

    /// The trigger device `id` left this machine: the leave debounce towards
    /// the host it leaves to, or `None` when it has no leave target (three
    /// hosts and no `leave_to`, see [`plan::target_for_leave`]).
    ///
    /// Both the idle case and a leave that interrupts an arrival debounce go
    /// through here, so the two cannot drift apart. What "nowhere to leave to"
    /// means is left to the caller: idle has no timer to cancel, an
    /// interrupted pull has.
    fn start_leave(&self, id: DeviceId, now: Instant) -> Option<(State, Action)> {
        let target = plan::target_for_leave(&self.cfg, &id)?;
        let deadline = now + self.debounce();
        Some((
            State::Confirming {
                target,
                trigger: id,
                deadline,
                from_arrival: false,
            },
            Action::ArmTimer(deadline),
        ))
    }

    /// Is `id` a device whose coming and going moves the screen? Followers
    /// never start a switch; they are only ever sent along by one.
    fn is_trigger(&self, id: &DeviceId) -> bool {
        self.cfg.device(id).is_some_and(|device| device.is_trigger)
    }

    /// `id` arrived while idle: may it start a pull debounce at all, or the
    /// reason it may not (spec §5)? Neither half of this asks about the screen:
    /// both answers hold whatever the monitor is showing, so an arrival that
    /// fails here costs the display nothing.
    fn pull_allowed(&self, id: &DeviceId) -> Result<(), &'static str> {
        if !self.is_trigger(id) {
            // A follower arriving means nothing: it goes where it is sent.
            return Err("not a trigger device");
        }
        if !self.cfg.options.pull_on_arrival {
            return Err("pulling on arrival is off");
        }
        Ok(())
    }

    /// The arrival debounce has expired: does the screen still need pulling
    /// home, or is it here already (spec §5)? The target is settled — a pull
    /// always targets this host — so this only ever says yes or no.
    fn pull_verdict(&mut self) -> Result<(), &'static str> {
        // One reading answers one pull, whatever we decide below.
        let observed = self.screen_here.take();
        match observed {
            // The monitor answered, so there is nothing left to guess at.
            // It is showing this Mac already — the usual round trip, where
            // the other side pushed the screen back before the keyboard got
            // here — and a second switch would only cost a cooldown that
            // swallows the user's next Easy-Switch press.
            Some(true) => Err("the screen is already here"),
            // The screen is on another host and the user's keyboard is here.
            // This holds even with no `last_target` at all: a Relay that has
            // just started used to sit still here, which is exactly the case
            // that left a freshly booted laptop without its screen.
            Some(false) => Ok(()),
            // The display would not say (or was never asked), so fall back to
            // the P3 rule, word for word.
            None => match self.last_target {
                // This machine has not switched anything since it started, so
                // it has no idea where the screen is. A guess would cost a
                // pointless switch and a cooldown that swallows the
                // Easy-Switch press the user makes next — exactly the wrong
                // thing right after a reboot, when the screen is on this Mac
                // anyway.
                None => Err("no switch to pull back from"),
                // The last switch already brought everything here.
                Some(target) if target == self.cfg.this_host => Err("the screen is already here"),
                Some(_) => Ok(()),
            },
        }
    }

    /// `id` arrived on this machine: it arms the pending return when it is a
    /// trigger device, nothing is armed yet, and there is something to come
    /// back from (`away`). The first device to arm it keeps it.
    fn arm_return(
        &self,
        returning: Option<DeviceId>,
        id: DeviceId,
        away: bool,
    ) -> Option<DeviceId> {
        match returning {
            Some(armed) => Some(armed),
            None if away && self.is_trigger(&id) => Some(id),
            None => None,
        }
    }

    fn debounce(&self) -> Duration {
        Duration::from_millis(self.cfg.timing.debounce_ms)
    }

    fn cooldown(&self) -> Duration {
        Duration::from_millis(self.cfg.timing.cooldown_ms)
    }
}

/// `id` left this machine: the pending return survives unless `id` is the very
/// device that armed it. Any other device — a second trigger, or a follower
/// whose nodes vanish with the switch we just ran — leaves the return alone.
fn clear_return(returning: Option<DeviceId>, id: &DeviceId) -> Option<DeviceId> {
    returning.filter(|armed| armed != id)
}

fn ignored_because(state: &State) -> &'static str {
    match state {
        State::Idle => "nothing to do while idle",
        State::Confirming { .. } => "waiting out the debounce",
        State::Switching { .. } => "a switch is in progress",
        State::Cooldown { .. } => "cooling down after a switch",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{three_host_config, two_host_config, KEYBOARD_ID, MOUSE_ID};

    fn keyboard() -> DeviceId {
        DeviceId(KEYBOARD_ID.to_string())
    }

    fn mouse() -> DeviceId {
        DeviceId(MOUSE_ID.to_string())
    }

    fn coordinator() -> (Coordinator, Instant) {
        (Coordinator::new(two_host_config()), Instant::now())
    }

    /// The id of the second trigger device added by
    /// [`coordinator_with_a_second_trigger`].
    const SECOND_TRIGGER_ID: &str = "046d:b365";

    fn second_trigger() -> DeviceId {
        DeviceId(SECOND_TRIGGER_ID.to_string())
    }

    /// The same, plus a second keyboard that is also a trigger. Two triggers
    /// are allowed; a trigger that also follows is not (see
    /// `ConfigError::TriggerCannotFollow`), so the "another trigger left"
    /// scenario is played by a second trigger device.
    fn coordinator_with_a_second_trigger() -> (Coordinator, Instant) {
        let mut cfg = two_host_config();
        let mut extra = cfg.devices[0].clone();
        extra.id = second_trigger();
        extra.name = "MX Keys Mini".to_string();
        cfg.devices.push(extra);
        cfg.validate().expect("two trigger devices are valid");
        (Coordinator::new(cfg), Instant::now())
    }

    /// Three hosts and no `leave_to`, so the keyboard has nowhere to leave to
    /// (see [`plan::target_for_leave`]). Built by hand rather than through
    /// `validate`, which rejects exactly this pairing today
    /// (`ConfigError::MissingLeaveTo`); the coordinator still carries the
    /// branch, and this is what reaches it.
    fn coordinator_without_a_leave_target() -> (Coordinator, Instant) {
        let mut cfg = three_host_config();
        cfg.devices[0].leave_to = None;
        (Coordinator::new(cfg), Instant::now())
    }

    /// The same, with `switch_back_on_reconnect` turned off.
    fn coordinator_without_switch_back() -> (Coordinator, Instant) {
        let mut cfg = two_host_config();
        cfg.options.switch_back_on_reconnect = false;
        (Coordinator::new(cfg), Instant::now())
    }

    /// The same, with `pull_on_arrival` turned off.
    fn coordinator_without_pull_on_arrival() -> (Coordinator, Instant) {
        let mut cfg = two_host_config();
        cfg.options.pull_on_arrival = false;
        (Coordinator::new(cfg), Instant::now())
    }

    /// Drives a coordinator to `Cooldown` and returns the moment the switch finished.
    fn into_cooldown(c: &mut Coordinator, now: Instant) -> Instant {
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        let fired = now + Duration::from_millis(800);
        c.handle(Event::TimerFired, fired);
        c.handle(Event::SwitchFinished, fired);
        fired
    }

    /// Drives a coordinator through one whole switch that hands this Mac over
    /// to the other host, and out the far end of the cooldown. Returns the
    /// moment it went idle; `last_target` then points at the other host.
    fn into_idle_after_a_switch_away(c: &mut Coordinator, now: Instant) -> Instant {
        let finished = into_cooldown(c, now);
        let idle = finished + Duration::from_millis(5000);
        c.handle(Event::TimerFired, idle);
        assert!(matches!(c.state(), State::Idle), "{:?}", c.state());
        idle
    }

    #[test]
    fn trigger_leaving_arms_the_debounce_timer() {
        let (mut c, now) = coordinator();
        let action = c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        let deadline = now + Duration::from_millis(800);
        assert_eq!(action, Action::ArmTimer(deadline));
        match c.state() {
            State::Confirming {
                target,
                trigger,
                deadline: d,
                from_arrival,
            } => {
                assert_eq!(*target, 1);
                assert_eq!(*trigger, keyboard());
                assert_eq!(*d, deadline);
                assert!(!*from_arrival, "this debounce came from a leave");
            }
            other => panic!("expected Confirming, got {other:?}"),
        }
    }

    #[test]
    fn a_non_trigger_device_leaving_does_nothing() {
        let (mut c, now) = coordinator();
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceLeft(DeviceId(MOUSE_ID.to_string()))),
            now,
        );
        assert!(matches!(action, Action::Ignore(_)));
        assert!(matches!(c.state(), State::Idle));
    }

    #[test]
    fn reconnecting_within_the_debounce_cancels_the_switch() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            now + Duration::from_millis(100),
        );
        assert_eq!(action, Action::CancelTimer);
        assert!(matches!(c.state(), State::Idle));
    }

    #[test]
    fn another_device_arriving_while_confirming_is_ignored() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(DeviceId(MOUSE_ID.to_string()))),
            now + Duration::from_millis(100),
        );
        assert!(matches!(action, Action::Ignore(_)));
        assert!(matches!(c.state(), State::Confirming { .. }));
    }

    #[test]
    fn the_debounce_expiring_starts_an_automatic_switch() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        let action = c.handle(Event::TimerFired, now + Duration::from_millis(800));
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 1,
                manual: false
            }
        );
        assert!(matches!(c.state(), State::Switching { target: 1, .. }));
    }

    #[test]
    fn a_finished_switch_enters_cooldown() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        match c.state() {
            State::Cooldown {
                until, returning, ..
            } => {
                assert_eq!(*until, finished + Duration::from_millis(5000));
                assert!(returning.is_none(), "nothing came back during this switch");
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }
    }

    #[test]
    fn cooldown_ignores_a_leaving_trigger_device() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceLeft(keyboard())),
            finished + Duration::from_millis(10),
        );
        assert!(matches!(action, Action::Ignore(_)));
        assert!(matches!(c.state(), State::Cooldown { .. }));
    }

    #[test]
    fn cooldown_ignores_a_manual_switch() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        let action = c.handle(
            Event::Trigger(TriggerEvent::Manual {
                target: 1,
                source: "tray",
            }),
            finished + Duration::from_millis(10),
        );
        assert!(matches!(action, Action::Ignore(_)));
        assert!(matches!(c.state(), State::Cooldown { .. }));
    }

    #[test]
    fn the_cooldown_timer_returns_to_idle() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        let action = c.handle(Event::TimerFired, finished + Duration::from_millis(5000));
        // The timer already fired; there is nothing left to cancel.
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    #[test]
    fn a_stale_timer_before_the_confirming_deadline_does_not_start_a_switch() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        let deadline = now + Duration::from_millis(800);
        let action = c.handle(Event::TimerFired, deadline - Duration::from_millis(1));
        assert_eq!(action, Action::Ignore("timer fired before deadline"));
        match c.state() {
            State::Confirming {
                target,
                deadline: d,
                ..
            } => {
                assert_eq!(*target, 1);
                assert_eq!(*d, deadline);
            }
            other => panic!("expected Confirming, got {other:?}"),
        }
    }

    #[test]
    fn a_timer_at_the_confirming_deadline_starts_a_switch() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        let deadline = now + Duration::from_millis(800);
        let action = c.handle(Event::TimerFired, deadline);
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 1,
                manual: false
            }
        );
        assert!(matches!(c.state(), State::Switching { target: 1, .. }));
    }

    #[test]
    fn a_stale_timer_before_the_cooldown_end_does_not_return_to_idle() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        let until = finished + Duration::from_millis(5000);
        let action = c.handle(Event::TimerFired, until - Duration::from_millis(1));
        assert_eq!(action, Action::Ignore("timer fired before cooldown end"));
        assert!(matches!(c.state(), State::Cooldown { .. }));
    }

    #[test]
    fn a_timer_at_the_cooldown_end_returns_to_idle() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        let until = finished + Duration::from_millis(5000);
        let action = c.handle(Event::TimerFired, until);
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    #[test]
    fn switching_ignores_a_manual_switch() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        c.handle(Event::TimerFired, now + Duration::from_millis(800));
        let action = c.handle(
            Event::Trigger(TriggerEvent::Manual {
                target: 0,
                source: "hotkey",
            }),
            now + Duration::from_millis(900),
        );
        assert!(matches!(action, Action::Ignore(_)));
        assert!(matches!(c.state(), State::Switching { target: 1, .. }));
    }

    #[test]
    fn a_manual_switch_skips_the_debounce() {
        let (mut c, now) = coordinator();
        let action = c.handle(
            Event::Trigger(TriggerEvent::Manual {
                target: 1,
                source: "cli",
            }),
            now,
        );
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 1,
                manual: true
            }
        );
        assert!(matches!(c.state(), State::Switching { target: 1, .. }));
    }

    #[test]
    fn a_manual_switch_to_an_undeclared_host_is_ignored() {
        let (mut c, now) = coordinator();
        let action = c.handle(
            Event::Trigger(TriggerEvent::Manual {
                target: 7,
                source: "cli",
            }),
            now,
        );
        assert_eq!(action, Action::Ignore("target not declared"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The bounce-back window: the keyboard failed to reach the other Mac and
    /// came home while the switch was still running.
    #[test]
    fn a_trigger_returning_during_the_switch_switches_back_when_the_cooldown_ends() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        c.handle(Event::TimerFired, now + Duration::from_millis(800));

        let arrived = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            now + Duration::from_millis(900),
        );
        assert!(matches!(arrived, Action::Ignore(_)));
        assert!(
            matches!(c.state(), State::Switching { target: 1, .. }),
            "the running switch is not interrupted: {:?}",
            c.state()
        );

        let finished = now + Duration::from_millis(1000);
        c.handle(Event::SwitchFinished, finished);
        match c.state() {
            State::Cooldown {
                until, returning, ..
            } => {
                assert_eq!(*until, finished + Duration::from_millis(5000));
                assert!(returning.is_some());
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let until = finished + Duration::from_millis(5000);
        let action = c.handle(Event::TimerFired, until);
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 0,
                manual: false
            }
        );
        assert!(matches!(c.state(), State::Switching { target: 0, .. }));
    }

    #[test]
    fn a_trigger_returning_does_not_switch_back_when_the_option_is_off() {
        let (mut c, now) = coordinator_without_switch_back();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        c.handle(Event::TimerFired, now + Duration::from_millis(800));
        c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            now + Duration::from_millis(900),
        );
        let finished = now + Duration::from_millis(1000);
        c.handle(Event::SwitchFinished, finished);

        let action = c.handle(Event::TimerFired, finished + Duration::from_millis(5000));
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    #[test]
    fn a_trigger_returning_during_the_cooldown_also_switches_back() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);

        let arrived = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            finished + Duration::from_millis(10),
        );
        assert!(matches!(arrived, Action::Ignore(_)));
        match c.state() {
            State::Cooldown { returning, .. } => assert!(returning.is_some()),
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let until = finished + Duration::from_millis(5000);
        let action = c.handle(Event::TimerFired, until);
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 0,
                manual: false
            }
        );
        assert!(matches!(c.state(), State::Switching { target: 0, .. }));
    }

    #[test]
    fn a_trigger_that_leaves_again_during_the_cooldown_clears_the_pending_return() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            finished + Duration::from_millis(10),
        );

        let left = c.handle(
            Event::Trigger(TriggerEvent::DeviceLeft(keyboard())),
            finished + Duration::from_millis(20),
        );
        assert!(matches!(left, Action::Ignore(_)));
        match c.state() {
            State::Cooldown { returning, .. } => {
                assert!(returning.is_none(), "it is not on this Mac any more")
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, finished + Duration::from_millis(5000));
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The pending return belongs to the device that armed it: a *second*
    /// trigger device dropping off this Mac during the cooldown must not
    /// cancel the bounce the first keyboard armed.
    #[test]
    fn another_trigger_device_leaving_does_not_clear_the_pending_return() {
        let (mut c, now) = coordinator_with_a_second_trigger();
        let finished = into_cooldown(&mut c, now);
        c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            finished + Duration::from_millis(10),
        );

        let left = c.handle(
            Event::Trigger(TriggerEvent::DeviceLeft(second_trigger())),
            finished + Duration::from_millis(20),
        );
        assert!(matches!(left, Action::Ignore(_)));
        match c.state() {
            State::Cooldown { returning, .. } => assert_eq!(
                returning.as_ref(),
                Some(&keyboard()),
                "the keyboard still armed the return"
            ),
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, finished + Duration::from_millis(5000));
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 0,
                manual: false
            }
        );
        assert!(matches!(c.state(), State::Switching { target: 0, .. }));
    }

    #[test]
    fn a_non_trigger_device_arriving_during_the_cooldown_does_not_switch_back() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);

        let arrived = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(mouse())),
            finished + Duration::from_millis(10),
        );
        assert!(matches!(arrived, Action::Ignore(_)));
        match c.state() {
            State::Cooldown { returning, .. } => assert!(returning.is_none()),
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, finished + Duration::from_millis(5000));
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    #[test]
    fn a_trigger_that_leaves_again_during_the_switch_clears_the_pending_return() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        c.handle(Event::TimerFired, now + Duration::from_millis(800));
        c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            now + Duration::from_millis(850),
        );
        c.handle(
            Event::Trigger(TriggerEvent::DeviceLeft(keyboard())),
            now + Duration::from_millis(900),
        );

        let finished = now + Duration::from_millis(1000);
        c.handle(Event::SwitchFinished, finished);
        match c.state() {
            State::Cooldown { returning, .. } => assert!(returning.is_none()),
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, finished + Duration::from_millis(5000));
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// A switch that targets this Mac has nothing to come back from, so the
    /// keyboard arriving afterwards must not schedule a second switch here.
    #[test]
    fn a_trigger_returning_after_a_switch_to_this_host_does_not_switch_again() {
        let (mut c, now) = coordinator();
        c.handle(
            Event::Trigger(TriggerEvent::Manual {
                target: 0,
                source: "tray",
            }),
            now,
        );
        let finished = now + Duration::from_millis(100);
        c.handle(Event::SwitchFinished, finished);

        let arrived = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            finished + Duration::from_millis(10),
        );
        assert!(matches!(arrived, Action::Ignore(_)));
        match c.state() {
            State::Cooldown { returning, .. } => {
                assert!(
                    returning.is_none(),
                    "the switch never went away from this Mac"
                )
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, finished + Duration::from_millis(5000));
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    #[test]
    fn a_stale_timer_keeps_the_pending_return_and_does_not_start_it() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            finished + Duration::from_millis(10),
        );

        let until = finished + Duration::from_millis(5000);
        let action = c.handle(Event::TimerFired, until - Duration::from_millis(1));
        assert_eq!(action, Action::Ignore("timer fired before cooldown end"));
        match c.state() {
            State::Cooldown { returning, .. } => {
                assert!(
                    returning.is_some(),
                    "the pending return survives a stale timer"
                )
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, until);
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 0,
                manual: false
            }
        );
        assert!(matches!(c.state(), State::Switching { target: 0, .. }));
    }

    #[test]
    fn a_non_trigger_device_arriving_during_the_switch_does_not_switch_back() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        c.handle(Event::TimerFired, now + Duration::from_millis(800));
        let arrived = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(mouse())),
            now + Duration::from_millis(900),
        );
        assert!(matches!(arrived, Action::Ignore(_)));

        let finished = now + Duration::from_millis(1000);
        c.handle(Event::SwitchFinished, finished);
        match c.state() {
            State::Cooldown { returning, .. } => assert!(returning.is_none()),
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, finished + Duration::from_millis(5000));
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The return switch is a switch like any other: it cools down and stops
    /// there, with no third switch in the chain.
    #[test]
    fn the_return_switch_settles_back_to_idle() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            finished + Duration::from_millis(10),
        );
        c.handle(Event::TimerFired, finished + Duration::from_millis(5000));

        let returned = finished + Duration::from_millis(5100);
        let action = c.handle(Event::SwitchFinished, returned);
        let until = returned + Duration::from_millis(5000);
        assert_eq!(action, Action::ArmTimer(until));
        match c.state() {
            State::Cooldown { returning, .. } => {
                assert!(returning.is_none(), "the return switch came home already")
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, until);
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The keyboard arrives and its debounce runs out: the action the expiry
    /// produced, which is where the pull decision is taken.
    ///
    /// The arrival itself only arms the timer now — the screen question waits
    /// for the end of the debounce — so every pull test goes through here.
    fn arrival_verdict(c: &mut Coordinator, now: Instant) -> Action {
        let armed = c.handle(Event::Trigger(TriggerEvent::DeviceArrived(keyboard())), now);
        let deadline = now + Duration::from_millis(800);
        assert_eq!(
            armed,
            Action::ArmTimer(deadline),
            "an arrival arms the debounce whatever the verdict turns out to be"
        );
        c.handle(Event::TimerFired, deadline)
    }

    /// A coordinator that has not switched anything yet knows nothing about
    /// where the screen is, so an arrival in `Idle` ends in a no-op (see
    /// [`a_trigger_arriving_right_after_a_start_does_not_pull`]).
    #[test]
    fn a_trigger_arriving_while_idle_never_switches_back() {
        let (mut c, now) = coordinator();
        let action = arrival_verdict(&mut c, now);
        assert!(matches!(action, Action::Ignore(_)));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The mirror of the leave rule (spec §5): the keyboard came back to this
    /// Mac by itself — the user pressed Easy-Switch, the other Mac either was
    /// not running Relay or never saw the keyboard leave — so this Mac pulls
    /// the screen and the following devices home.
    #[test]
    fn a_trigger_arriving_after_a_switch_away_pulls_everything_home() {
        let (mut c, now) = coordinator();
        let idle = into_idle_after_a_switch_away(&mut c, now);

        let arrived = idle + Duration::from_millis(10);
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            arrived,
        );
        let deadline = arrived + Duration::from_millis(800);
        assert_eq!(action, Action::ArmTimer(deadline));
        match c.state() {
            State::Confirming {
                target,
                trigger,
                deadline: d,
                from_arrival,
            } => {
                assert_eq!(*target, 0, "the pull targets this Mac");
                assert_eq!(*trigger, keyboard());
                assert_eq!(*d, deadline);
                assert!(*from_arrival, "this debounce came from an arrival");
            }
            other => panic!("expected Confirming, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, deadline);
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 0,
                manual: false
            }
        );
        assert!(matches!(c.state(), State::Switching { target: 0, .. }));
    }

    #[test]
    fn a_trigger_arriving_does_not_pull_when_the_option_is_off() {
        let (mut c, now) = coordinator_without_pull_on_arrival();
        let idle = into_idle_after_a_switch_away(&mut c, now);

        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            idle + Duration::from_millis(10),
        );
        assert_eq!(action, Action::Ignore("pulling on arrival is off"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// A Relay that just started has no idea where the screen is: pulling on a
    /// guess would cost a needless switch and a cooldown that swallows the
    /// user's next Easy-Switch press.
    #[test]
    fn a_trigger_arriving_right_after_a_start_does_not_pull() {
        let (mut c, now) = coordinator();
        let action = arrival_verdict(&mut c, now);
        assert_eq!(action, Action::Ignore("no switch to pull back from"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The last switch already targeted this Mac, so the screen is here.
    #[test]
    fn a_trigger_arriving_after_a_switch_to_this_host_does_not_pull() {
        let (mut c, now) = coordinator();
        c.handle(
            Event::Trigger(TriggerEvent::Manual {
                target: 0,
                source: "tray",
            }),
            now,
        );
        let finished = now + Duration::from_millis(100);
        c.handle(Event::SwitchFinished, finished);
        let idle = finished + Duration::from_millis(5000);
        c.handle(Event::TimerFired, idle);
        assert!(matches!(c.state(), State::Idle));

        let action = arrival_verdict(&mut c, idle + Duration::from_millis(10));
        assert_eq!(action, Action::Ignore("the screen is already here"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The debounce is symmetric in that the same device turning round
    /// cancels the pull — but a keyboard that leaves again really has left.
    /// The user pressed Easy-Switch within the 800 ms, so the screen and the
    /// following devices must go after it instead of being stranded here.
    #[test]
    fn leaving_again_within_the_debounce_switches_away_instead_of_pulling() {
        let (mut c, now) = coordinator();
        let idle = into_idle_after_a_switch_away(&mut c, now);
        let arrived = idle + Duration::from_millis(10);
        c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            arrived,
        );

        let left = arrived + Duration::from_millis(100);
        let action = c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), left);
        let deadline = left + Duration::from_millis(800);
        assert_eq!(
            action,
            Action::ArmTimer(deadline),
            "the leave starts a debounce of its own"
        );
        match c.state() {
            State::Confirming {
                target,
                trigger,
                deadline: d,
                from_arrival,
            } => {
                assert_eq!(*target, 1, "the switch follows the keyboard away");
                assert_eq!(*trigger, keyboard());
                assert_eq!(*d, deadline);
                assert!(!*from_arrival, "the pull gave way to a leave");
            }
            other => panic!("expected Confirming, got {other:?}"),
        }

        let action = c.handle(Event::TimerFired, deadline);
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 1,
                manual: false
            }
        );
    }

    /// The same leave with nowhere to go: there is no switch to start, so all
    /// that is left is to drop the pull.
    #[test]
    fn leaving_again_with_no_leave_target_only_cancels_the_pull() {
        let (mut c, now) = coordinator_without_a_leave_target();
        c.handle(Event::Trigger(TriggerEvent::DeviceArrived(keyboard())), now);
        assert!(matches!(
            c.state(),
            State::Confirming {
                from_arrival: true,
                ..
            }
        ));

        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceLeft(keyboard())),
            now + Duration::from_millis(100),
        );
        assert_eq!(action, Action::CancelTimer);
        assert!(matches!(c.state(), State::Idle));
    }

    /// Only the device that armed the pull can end it: another trigger going
    /// away leaves the debounce running as before.
    #[test]
    fn a_different_device_leaving_within_a_pull_debounce_changes_nothing() {
        let (mut c, now) = coordinator_with_a_second_trigger();
        c.handle(Event::Trigger(TriggerEvent::DeviceArrived(keyboard())), now);
        let deadline = now + Duration::from_millis(800);

        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceLeft(second_trigger())),
            now + Duration::from_millis(100),
        );
        assert_eq!(action, Action::Ignore("waiting out the debounce"));
        match c.state() {
            State::Confirming {
                target,
                trigger,
                deadline: d,
                from_arrival,
            } => {
                assert_eq!(*target, 0, "the pull still targets this Mac");
                assert_eq!(*trigger, keyboard());
                assert_eq!(*d, deadline, "the original deadline stands");
                assert!(*from_arrival, "this debounce still came from an arrival");
            }
            other => panic!("expected Confirming, got {other:?}"),
        }
    }

    #[test]
    fn a_non_trigger_device_arriving_while_idle_does_not_pull() {
        let (mut c, now) = coordinator();
        let idle = into_idle_after_a_switch_away(&mut c, now);

        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(mouse())),
            idle + Duration::from_millis(10),
        );
        assert_eq!(action, Action::Ignore("not a trigger device"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// Once the pull has run, the screen is here and the last target is this
    /// Mac: a second arrival must not start another switch.
    #[test]
    fn a_second_arrival_after_a_pull_does_not_pull_again() {
        let (mut c, now) = coordinator();
        let idle = into_idle_after_a_switch_away(&mut c, now);
        let arrived = idle + Duration::from_millis(10);
        c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            arrived,
        );
        let deadline = arrived + Duration::from_millis(800);
        c.handle(Event::TimerFired, deadline);

        let finished = deadline + Duration::from_millis(100);
        c.handle(Event::SwitchFinished, finished);
        let back_to_idle = finished + Duration::from_millis(5000);
        c.handle(Event::TimerFired, back_to_idle);
        assert!(matches!(c.state(), State::Idle));

        let action = arrival_verdict(&mut c, back_to_idle + Duration::from_millis(10));
        assert_eq!(action, Action::Ignore("the screen is already here"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The other direction keeps its own cancel rule: a leave-direction
    /// debounce is still cancelled by the device coming back, and a *leave*
    /// during it (a repeated notification) does not cancel anything.
    #[test]
    fn leaving_again_within_a_leave_debounce_does_not_cancel_the_switch() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceLeft(keyboard())),
            now + Duration::from_millis(100),
        );
        assert_eq!(action, Action::Ignore("waiting out the debounce"));
        assert!(matches!(c.state(), State::Confirming { .. }));

        let action = c.handle(Event::TimerFired, now + Duration::from_millis(800));
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 1,
                manual: false
            }
        );
    }

    /// And the arrival direction ignores a repeated arrival the same way.
    #[test]
    fn arriving_again_within_a_pull_debounce_does_not_cancel_it() {
        let (mut c, now) = coordinator();
        let idle = into_idle_after_a_switch_away(&mut c, now);
        let arrived = idle + Duration::from_millis(10);
        c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            arrived,
        );

        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            arrived + Duration::from_millis(100),
        );
        assert_eq!(action, Action::Ignore("waiting out the debounce"));
        assert!(matches!(c.state(), State::Confirming { .. }));
    }

    /// The pull rule lives in `Idle` only: P1 still owns the arrivals that
    /// happen inside this machine's own switch window.
    #[test]
    fn a_pull_does_not_disturb_the_bounce_back_window() {
        let (mut c, now) = coordinator();
        let finished = into_cooldown(&mut c, now);
        let arrived = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            finished + Duration::from_millis(10),
        );
        assert_eq!(arrived, Action::Ignore("cooling down after a switch"));

        // One return switch, not two: the cooldown end starts it, and the
        // arrival that armed it is long past by the time we are idle again.
        let until = finished + Duration::from_millis(5000);
        let action = c.handle(Event::TimerFired, until);
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 0,
                manual: false
            }
        );
        let returned = until + Duration::from_millis(100);
        c.handle(Event::SwitchFinished, returned);
        let action = c.handle(Event::TimerFired, returned + Duration::from_millis(5000));
        assert_eq!(action, Action::Ignore("cooldown finished"));
        assert!(matches!(c.state(), State::Idle));
    }

    #[test]
    fn a_stray_timer_in_idle_is_ignored() {
        let (mut c, now) = coordinator();
        let action = c.handle(Event::TimerFired, now);
        assert!(matches!(action, Action::Ignore(_)));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The case that used to fail: a Relay that has just started knows nothing
    /// about its own past, but the monitor does. It says the screen is on
    /// another host, so the keyboard's arrival pulls it home even though
    /// `last_target` is `None`.
    #[test]
    fn the_screen_being_elsewhere_pulls_it_home_without_any_past_switch() {
        let (mut c, now) = coordinator();

        let action = c.handle(Event::Trigger(TriggerEvent::DeviceArrived(keyboard())), now);
        let deadline = now + Duration::from_millis(800);
        assert_eq!(action, Action::ArmTimer(deadline));
        match c.state() {
            State::Confirming {
                target,
                from_arrival,
                ..
            } => {
                assert_eq!(*target, 0, "the pull targets this Mac");
                assert!(*from_arrival, "this debounce came from an arrival");
            }
            other => panic!("expected Confirming, got {other:?}"),
        }

        // The runtime asks the monitor as the debounce runs out, not back when
        // the keyboard turned up.
        c.observe_screen(Some(false));
        let action = c.handle(Event::TimerFired, deadline);
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 0,
                manual: false
            }
        );
    }

    /// The other half of the same trade: the normal round trip. The other Mac
    /// already sent the screen back, so the keyboard arriving here must not
    /// buy a second switch and a five-second cooldown — even though this
    /// machine's own last switch pointed away.
    #[test]
    fn the_screen_being_here_beats_a_last_target_that_points_away() {
        let (mut c, now) = coordinator();
        let idle = into_idle_after_a_switch_away(&mut c, now);

        let arrived = idle + Duration::from_millis(10);
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            arrived,
        );
        let deadline = arrived + Duration::from_millis(800);
        assert_eq!(action, Action::ArmTimer(deadline));

        c.observe_screen(Some(true));
        let action = c.handle(Event::TimerFired, deadline);
        assert_eq!(action, Action::Ignore("the screen is already here"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// A display that will not answer changes nothing: the P3 rule decides,
    /// word for word. Both directions of it.
    #[test]
    fn an_unreadable_display_leaves_the_last_target_rule_in_charge() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceArrived(keyboard())), now);
        c.observe_screen(None);
        let action = c.handle(Event::TimerFired, now + Duration::from_millis(800));
        assert_eq!(action, Action::Ignore("no switch to pull back from"));
        assert!(matches!(c.state(), State::Idle));

        let (mut c, now) = coordinator();
        let idle = into_idle_after_a_switch_away(&mut c, now);
        let arrived = idle + Duration::from_millis(10);
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            arrived,
        );
        let deadline = arrived + Duration::from_millis(800);
        assert_eq!(action, Action::ArmTimer(deadline));

        c.observe_screen(None);
        let action = c.handle(Event::TimerFired, deadline);
        assert_eq!(
            action,
            Action::StartSwitch {
                target: 0,
                manual: false
            },
            "the last switch pointed away, so the fallback still pulls"
        );
    }

    /// The option still comes first: with the pull turned off, what the
    /// monitor says is beside the point.
    #[test]
    fn an_observation_does_not_revive_the_pull_when_the_option_is_off() {
        let (mut c, now) = coordinator_without_pull_on_arrival();
        c.observe_screen(Some(false));
        let action = c.handle(Event::Trigger(TriggerEvent::DeviceArrived(keyboard())), now);
        assert_eq!(action, Action::Ignore("pulling on arrival is off"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// One observation answers one pull. The runtime reads the monitor just
    /// before the debounce expires, so the answer that decided one pull must
    /// not still be lying around to decide the next.
    #[test]
    fn an_observation_is_used_once_and_then_forgotten() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceArrived(keyboard())), now);
        c.observe_screen(Some(false));
        let deadline = now + Duration::from_millis(800);
        assert_eq!(
            c.handle(Event::TimerFired, deadline),
            Action::StartSwitch {
                target: 0,
                manual: false
            }
        );

        // That pull runs and cools down; the screen is here now.
        let finished = deadline + Duration::from_millis(100);
        c.handle(Event::SwitchFinished, finished);
        let idle = finished + Duration::from_millis(5000);
        c.handle(Event::TimerFired, idle);
        assert!(matches!(c.state(), State::Idle));

        // A second arrival with no fresh reading falls back to `last_target`,
        // which points here. A stale `Some(false)` would pull all over again.
        let action = arrival_verdict(&mut c, idle + Duration::from_millis(10));
        assert_eq!(action, Action::Ignore("the screen is already here"));
        assert!(matches!(c.state(), State::Idle));
    }

    /// The observation only ever reaches the `Idle` pull. P1's bounce-back —
    /// an arrival during this machine's own switch or cooldown — keeps its own
    /// rule, whatever the monitor happens to be showing at the time.
    #[test]
    fn an_observation_does_not_disturb_the_bounce_back_window() {
        let (mut c, now) = coordinator();
        c.handle(Event::Trigger(TriggerEvent::DeviceLeft(keyboard())), now);
        let fired = now + Duration::from_millis(800);
        c.handle(Event::TimerFired, fired);

        // Mid-switch the screen is (still) on the other Mac; the arrival is
        // ignored all the same, and arms the return.
        c.observe_screen(Some(true));
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            fired + Duration::from_millis(10),
        );
        assert_eq!(action, Action::Ignore("a switch is in progress"));

        c.handle(Event::SwitchFinished, fired + Duration::from_millis(20));
        c.observe_screen(Some(true));
        let arrived = fired + Duration::from_millis(30);
        let action = c.handle(
            Event::Trigger(TriggerEvent::DeviceArrived(keyboard())),
            arrived,
        );
        assert_eq!(action, Action::Ignore("cooling down after a switch"));

        // The return still runs when the cooldown ends.
        let until = fired + Duration::from_millis(5020);
        assert_eq!(
            c.handle(Event::TimerFired, until),
            Action::StartSwitch {
                target: 0,
                manual: false
            }
        );
    }
}
