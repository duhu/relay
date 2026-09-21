//! Where switches come from: HID presence.
//!
//! [`presence`] is the macOS side (IOHIDManager on its own run loop); this
//! module is the pure half that turns its raw node events into the
//! [`TriggerEvent`]s the coordinator understands.

pub mod presence;

use std::collections::HashMap;

use crate::types::{DeviceId, TriggerEvent};

#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    #[error("cannot start the HID watcher thread: {0}")]
    Thread(String),
}

/// One HID node appearing or disappearing.
///
/// A node is a single `IOHIDDevice`; one Bluetooth keyboard publishes several
/// of them (keyboard, consumer control, vendor pages, ...), which is why
/// [`PresenceTracker`] exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RawHidEvent {
    Added {
        /// The `IOHIDDevice` pointer, which is stable between the matching and
        /// the removal callback.
        node: usize,
        vid: u16,
        pid: u16,
        product: String,
    },
    Removed {
        node: usize,
    },
}

/// Folds the HID nodes of a configured device into one arrival and one
/// departure.
///
/// Only devices whose `vid:pid` is in `watched` are *reported*, but presence is
/// remembered for every node on the bus: the watched list comes from the
/// config, and a config that arrives late (or a reload) must not leave the
/// tracker blind to devices that are already here. See [`Self::set_watched`].
pub struct PresenceTracker {
    /// `(vid, pid)` of every watched device, in configuration order.
    by_key: Vec<((u16, u16), DeviceId)>,
    /// The `(vid, pid)` of every live node, watched or not.
    nodes: HashMap<usize, (u16, u16)>,
}

impl PresenceTracker {
    pub fn new(watched: Vec<DeviceId>) -> Self {
        Self {
            by_key: keys(watched),
            nodes: HashMap::new(),
        }
    }

    /// Replaces the devices this tracker reports on, keeping every node it has
    /// already seen, so a reload never loses presence state.
    pub fn set_watched(&mut self, watched: Vec<DeviceId>) {
        self.by_key = keys(watched);
    }

    /// Applies one raw event, returning the trigger it caused, if any.
    pub fn feed(&mut self, ev: RawHidEvent) -> Option<TriggerEvent> {
        match ev {
            RawHidEvent::Added { node, vid, pid, .. } => {
                if self.nodes.contains_key(&node) {
                    // The same node twice: nothing changed.
                    return None;
                }
                self.nodes.insert(node, (vid, pid));
                let id = self.device_for(vid, pid)?.clone();
                (self.live(vid, pid) == 1).then_some(TriggerEvent::DeviceArrived(id))
            }
            RawHidEvent::Removed { node } => {
                let (vid, pid) = self.nodes.remove(&node)?;
                let id = self.device_for(vid, pid)?.clone();
                (self.live(vid, pid) == 0).then_some(TriggerEvent::DeviceLeft(id))
            }
        }
    }

    /// How many nodes of `(vid, pid)` are live; only 0↔1 crossings are events.
    fn live(&self, vid: u16, pid: u16) -> usize {
        self.nodes
            .values()
            .filter(|key| **key == (vid, pid))
            .count()
    }

    fn device_for(&self, vid: u16, pid: u16) -> Option<&DeviceId> {
        self.by_key
            .iter()
            .find(|(key, _)| *key == (vid, pid))
            .map(|(_, id)| id)
    }
}

/// The watched ids keyed by `vid:pid`; ids not in that form can never match.
fn keys(watched: Vec<DeviceId>) -> Vec<((u16, u16), DeviceId)> {
    watched
        .into_iter()
        .filter_map(|id| id.vid_pid().map(|key| (key, id)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYBOARD: &str = "046d:b366";
    const MOUSE: &str = "046d:b023";

    fn keyboard() -> DeviceId {
        DeviceId(KEYBOARD.to_string())
    }

    fn tracker() -> PresenceTracker {
        PresenceTracker::new(vec![keyboard(), DeviceId(MOUSE.to_string())])
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

    #[test]
    fn the_first_node_of_a_watched_device_is_an_arrival() {
        let mut tracker = tracker();
        assert_eq!(
            tracker.feed(added(1, KEYBOARD)),
            Some(TriggerEvent::DeviceArrived(keyboard()))
        );
    }

    #[test]
    fn the_later_nodes_of_the_same_device_are_silent() {
        let mut tracker = tracker();
        tracker.feed(added(1, KEYBOARD));
        for node in 2..=5 {
            assert_eq!(tracker.feed(added(node, KEYBOARD)), None, "node {node}");
        }
    }

    #[test]
    fn only_the_last_node_leaving_is_a_departure() {
        let mut tracker = tracker();
        for node in 1..=5 {
            tracker.feed(added(node, KEYBOARD));
        }
        for node in 1..=4 {
            assert_eq!(
                tracker.feed(RawHidEvent::Removed { node }),
                None,
                "node {node}"
            );
        }
        assert_eq!(
            tracker.feed(RawHidEvent::Removed { node: 5 }),
            Some(TriggerEvent::DeviceLeft(keyboard()))
        );
    }

    #[test]
    fn a_device_that_comes_back_arrives_again() {
        let mut tracker = tracker();
        tracker.feed(added(1, KEYBOARD));
        tracker.feed(RawHidEvent::Removed { node: 1 });
        assert_eq!(
            tracker.feed(added(2, KEYBOARD)),
            Some(TriggerEvent::DeviceArrived(keyboard()))
        );
    }

    #[test]
    fn devices_are_counted_separately() {
        let mut tracker = tracker();
        tracker.feed(added(1, KEYBOARD));
        assert_eq!(
            tracker.feed(added(2, MOUSE)),
            Some(TriggerEvent::DeviceArrived(DeviceId(MOUSE.to_string())))
        );
        assert_eq!(
            tracker.feed(RawHidEvent::Removed { node: 1 }),
            Some(TriggerEvent::DeviceLeft(keyboard())),
            "the mouse's node must not keep the keyboard alive"
        );
    }

    #[test]
    fn an_unwatched_device_produces_nothing() {
        let mut tracker = tracker();
        assert_eq!(tracker.feed(added(1, "dead:beef")), None);
        assert_eq!(tracker.feed(RawHidEvent::Removed { node: 1 }), None);
    }

    #[test]
    fn a_repeated_node_id_does_not_double_count() {
        let mut tracker = tracker();
        tracker.feed(added(1, KEYBOARD));
        assert_eq!(tracker.feed(added(1, KEYBOARD)), None);
        assert_eq!(
            tracker.feed(RawHidEvent::Removed { node: 1 }),
            Some(TriggerEvent::DeviceLeft(keyboard())),
            "one arrival, one departure"
        );
    }

    #[test]
    fn set_watched_keeps_the_nodes_seen_while_nothing_was_watched() {
        // A core that started without a usable config watches nothing, but the
        // devices are already on the bus.
        let mut tracker = PresenceTracker::new(Vec::new());
        for node in 1..=5 {
            assert_eq!(tracker.feed(added(node, KEYBOARD)), None, "node {node}");
        }

        tracker.set_watched(vec![keyboard()]);

        for node in 1..=4 {
            assert_eq!(
                tracker.feed(RawHidEvent::Removed { node }),
                None,
                "node {node}"
            );
        }
        assert_eq!(
            tracker.feed(RawHidEvent::Removed { node: 5 }),
            Some(TriggerEvent::DeviceLeft(keyboard())),
            "the nodes seen before the config still count"
        );
    }

    #[test]
    fn a_removal_of_an_unknown_node_is_ignored() {
        let mut tracker = tracker();
        assert_eq!(tracker.feed(RawHidEvent::Removed { node: 99 }), None);
    }

    #[test]
    fn a_device_id_without_a_vid_pid_never_matches() {
        let mut tracker = PresenceTracker::new(vec![DeviceId("mx-keyboard".to_string())]);
        assert_eq!(tracker.feed(added(1, KEYBOARD)), None);
    }
}
