//! "Finished, not yet seen": which idle sessions just completed work the user
//! hasn't looked at.
//!
//! The moment that matters most is a session going from working to idle. A
//! session idle for three hours and one that finished ten seconds ago land in
//! the same Idle column, though only the second wants the user. A session is
//! flagged when it leaves running or permission for idle, and the flag stays
//! until the user focuses the session from the overlay, clicks its card, or
//! the session becomes active again.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A session must have been active this long before going idle counts as
/// finishing. Scraped status can flip to running for a poll or two when a
/// pane repaints; that is not work completing.
const MIN_ACTIVE: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Track {
    /// When the current stretch of running/permission began.
    active_since: Option<Instant>,
    finished: bool,
}

#[derive(Default)]
pub struct Tracker {
    sessions: HashMap<String, Track>,
}

impl Tracker {
    /// Record this poll's status for `id` and return whether it is finished
    /// and unseen. A session first seen idle is not flagged: nothing says it
    /// finished while the overlay was watching.
    pub fn observe(&mut self, id: &str, status: &str, now: Instant) -> bool {
        let track = self.sessions.entry(id.to_string()).or_default();
        if matches!(status, "running" | "permission") {
            track.active_since.get_or_insert(now);
            track.finished = false;
        } else if let Some(since) = track.active_since.take() {
            if now.duration_since(since) >= MIN_ACTIVE {
                track.finished = true;
            }
        }
        track.finished
    }

    pub fn mark_seen(&mut self, id: &str) {
        if let Some(track) = self.sessions.get_mut(id) {
            track.finished = false;
        }
    }

    /// Forget sessions that no longer exist.
    pub fn retain(&mut self, live: &HashSet<&str>) {
        self.sessions.retain(|id, _| live.contains(id.as_str()));
    }
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);

/// Set `finished` on every session from this poll.
pub fn update(sessions: &mut [crate::tmux::AgentSession]) {
    let mut guard = TRACKER.lock().unwrap();
    let tracker = guard.get_or_insert_with(Tracker::default);
    let now = Instant::now();
    for s in sessions.iter_mut() {
        s.finished = tracker.observe(&s.session_id, &s.status, now);
    }
    let live: HashSet<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
    tracker.retain(&live);
}

pub fn mark_seen(id: &str) {
    if let Some(tracker) = TRACKER.lock().unwrap().as_mut() {
        tracker.mark_seen(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn work_ending_flags_the_session_until_seen() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert!(!t.observe("AO-01", "running", t0));
        assert!(t.observe("AO-01", "idle", t0 + secs(30)));
        assert!(t.observe("AO-01", "idle", t0 + secs(90)), "flag must persist");
        t.mark_seen("AO-01");
        assert!(!t.observe("AO-01", "idle", t0 + secs(91)));
    }

    #[test]
    fn a_session_first_seen_idle_is_not_flagged() {
        let mut t = Tracker::default();
        assert!(!t.observe("AO-02", "idle", Instant::now()));
    }

    #[test]
    fn a_brief_flicker_to_running_is_not_finishing() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe("AO-03", "idle", t0);
        t.observe("AO-03", "running", t0 + secs(1));
        assert!(!t.observe("AO-03", "idle", t0 + secs(2)));
    }

    #[test]
    fn becoming_active_again_clears_the_flag() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe("AO-04", "running", t0);
        assert!(t.observe("AO-04", "idle", t0 + secs(10)));
        assert!(!t.observe("AO-04", "running", t0 + secs(20)));
    }

    /// A denied approval ends the turn: permission → idle is finishing too.
    #[test]
    fn leaving_an_approval_for_idle_counts() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe("AO-05", "permission", t0);
        assert!(t.observe("AO-05", "idle", t0 + secs(40)));
    }

    #[test]
    fn ended_sessions_are_forgotten() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe("AO-06", "running", t0);
        t.retain(&HashSet::new());
        // Same id seen again starts from scratch rather than inheriting state.
        assert!(!t.observe("AO-06", "idle", t0 + secs(10)));
    }
}
