//! Windowing support — tumbling, sliding, and session windows.
//!
//! Windows group events by time ranges. Each window has:
//! - A start and end timestamp (nanoseconds since epoch)
//! - A collection of events assigned to that window
//! - A watermark that tracks progress through event time
//!
//! Late events (events with timestamp < watermark) are handled according to
//! the configured `LatePolicy`.

use std::collections::BTreeMap;

/// How to handle events that arrive after the watermark has passed their window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatePolicy {
    /// Silently discard late events.
    Discard,
    /// Emit late events to a side output for separate handling.
    SideOutput,
    /// Re-compute the window including the late event.
    Recompute,
}

/// A time window with start (inclusive) and end (exclusive) timestamps in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Window {
    /// Window start (inclusive), nanoseconds since epoch.
    pub start: i64,
    /// Window end (exclusive), nanoseconds since epoch.
    pub end: i64,
}

impl Window {
    pub fn new(start: i64, end: i64) -> Self {
        Self { start, end }
    }

    /// Check if a timestamp falls within this window.
    pub fn contains(&self, timestamp: i64) -> bool {
        timestamp >= self.start && timestamp < self.end
    }

    /// Duration of the window in nanoseconds.
    pub fn duration_ns(&self) -> i64 {
        self.end - self.start
    }
}

/// Watermark tracker — tracks the progress of event time.
///
/// The watermark is a timestamp assertion: "no more events with timestamp
/// less than this watermark will arrive." Used to trigger window closing.
#[derive(Debug)]
pub struct Watermark {
    /// Current watermark value (nanos since epoch).
    value: i64,
    /// Maximum allowed lateness before an event is considered "late" (nanos).
    max_lateness: i64,
}

impl Watermark {
    /// Create a watermark with the given max allowed lateness.
    pub fn new(max_lateness_ns: i64) -> Self {
        Self {
            value: i64::MIN,
            max_lateness: max_lateness_ns,
        }
    }

    /// Current watermark value.
    pub fn value(&self) -> i64 {
        self.value
    }

    /// Advance the watermark based on an observed event timestamp.
    /// The watermark = max(observed timestamps) - max_lateness.
    pub fn advance(&mut self, event_timestamp: i64) {
        let candidate = event_timestamp - self.max_lateness;
        if candidate > self.value {
            self.value = candidate;
        }
    }

    /// Check if an event is late (its timestamp is before the watermark).
    pub fn is_late(&self, event_timestamp: i64) -> bool {
        event_timestamp < self.value
    }
}

/// Tumbling window assigner — non-overlapping, fixed-size windows.
///
/// Each event is assigned to exactly one window based on its timestamp.
/// Windows are aligned to the epoch (window_start = timestamp - (timestamp % size)).
pub struct TumblingWindows {
    /// Window size in nanoseconds.
    size_ns: i64,
}

impl TumblingWindows {
    /// Create a tumbling window assigner with the given window size.
    pub fn new(size_ns: i64) -> Self {
        assert!(size_ns > 0, "window size must be positive");
        Self { size_ns }
    }

    /// Create from milliseconds.
    pub fn from_millis(ms: i64) -> Self {
        Self::new(ms * 1_000_000)
    }

    /// Create from seconds.
    pub fn from_secs(secs: i64) -> Self {
        Self::new(secs * 1_000_000_000)
    }

    /// Assign a timestamp to its window.
    pub fn assign(&self, timestamp: i64) -> Window {
        let start = timestamp - timestamp.rem_euclid(self.size_ns);
        Window::new(start, start + self.size_ns)
    }
}

/// Session window assigner — windows defined by activity gaps.
///
/// A session window closes when no new events arrive within the `gap` duration.
/// Each key can have its own active session.
pub struct SessionWindows {
    /// Maximum gap between events in a session (nanoseconds).
    gap_ns: i64,
}

impl SessionWindows {
    pub fn new(gap_ns: i64) -> Self {
        assert!(gap_ns > 0, "session gap must be positive");
        Self { gap_ns }
    }

    pub fn from_millis(ms: i64) -> Self {
        Self::new(ms * 1_000_000)
    }

    pub fn from_secs(secs: i64) -> Self {
        Self::new(secs * 1_000_000_000)
    }

    /// Gap duration in nanoseconds.
    pub fn gap(&self) -> i64 {
        self.gap_ns
    }
}

/// Tracks active session windows per key.
///
/// When a new event arrives for a key:
/// - If an active session exists and the event is within the gap: extend the session
/// - If the gap has elapsed: close the old session, start a new one
/// - If no session exists: start a new one
pub struct SessionTracker {
    config: SessionWindows,
    /// Active sessions: key → (window_start, last_event_timestamp)
    sessions: BTreeMap<Vec<u8>, (i64, i64)>,
}

impl SessionTracker {
    pub fn new(config: SessionWindows) -> Self {
        Self {
            config,
            sessions: BTreeMap::new(),
        }
    }

    /// Process an event for a key. Returns:
    /// - `SessionEvent::Extended` if the event extended an existing session
    /// - `SessionEvent::Opened` if a new session was started
    /// - `SessionEvent::ClosedAndOpened(Window)` if an old session was closed and a new one opened
    pub fn on_event(&mut self, key: &[u8], timestamp: i64) -> SessionEvent {
        if let Some((start, last_ts)) = self.sessions.get_mut(key) {
            if timestamp - *last_ts <= self.config.gap_ns {
                // Within gap — extend session
                *last_ts = timestamp;
                SessionEvent::Extended
            } else {
                // Gap exceeded — close old session, open new one
                let closed = Window::new(*start, *last_ts + self.config.gap_ns);
                *start = timestamp;
                *last_ts = timestamp;
                SessionEvent::ClosedAndOpened(closed)
            }
        } else {
            // No active session — start new one
            self.sessions.insert(key.to_vec(), (timestamp, timestamp));
            SessionEvent::Opened
        }
    }

    /// Close all sessions that have expired (last event older than watermark - gap).
    /// Returns the closed windows.
    pub fn close_expired(&mut self, watermark: i64) -> Vec<(Vec<u8>, Window)> {
        let threshold = watermark - self.config.gap_ns;
        let mut closed = Vec::new();

        self.sessions.retain(|key, (start, last_ts)| {
            if *last_ts < threshold {
                closed.push((
                    key.clone(),
                    Window::new(*start, *last_ts + self.config.gap_ns),
                ));
                false
            } else {
                true
            }
        });

        closed
    }

    /// Number of active sessions.
    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }
}

/// Result of processing an event in a session window.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// A new session was opened.
    Opened,
    /// An existing session was extended.
    Extended,
    /// An old session was closed (returned) and a new one was opened.
    ClosedAndOpened(Window),
}

/// Sliding window assigner — overlapping windows at regular intervals.
///
/// Each event can be assigned to multiple windows.
/// - `size_ns`: window duration
/// - `slide_ns`: interval between window starts (must be <= size_ns)
pub struct SlidingWindows {
    size_ns: i64,
    slide_ns: i64,
}

impl SlidingWindows {
    pub fn new(size_ns: i64, slide_ns: i64) -> Self {
        assert!(size_ns > 0, "window size must be positive");
        assert!(slide_ns > 0, "slide must be positive");
        assert!(slide_ns <= size_ns, "slide must be <= window size");
        Self { size_ns, slide_ns }
    }

    pub fn from_millis(size_ms: i64, slide_ms: i64) -> Self {
        Self::new(size_ms * 1_000_000, slide_ms * 1_000_000)
    }

    /// Assign a timestamp to all windows it belongs to.
    pub fn assign(&self, timestamp: i64) -> Vec<Window> {
        let mut windows = Vec::new();
        // Find the latest window start <= timestamp
        let last_start = timestamp - timestamp.rem_euclid(self.slide_ns);

        // Walk backward to find all windows containing this timestamp
        let mut start = last_start;
        while start + self.size_ns > timestamp {
            windows.push(Window::new(start, start + self.size_ns));
            start -= self.slide_ns;
            if start < 0 {
                break;
            }
        }

        windows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS_PER_SEC: i64 = 1_000_000_000;

    #[test]
    fn window_contains() {
        let w = Window::new(100, 200);
        assert!(w.contains(100)); // inclusive start
        assert!(w.contains(150));
        assert!(!w.contains(200)); // exclusive end
        assert!(!w.contains(99));
    }

    #[test]
    fn tumbling_window_assignment() {
        let tw = TumblingWindows::from_secs(10);

        let w1 = tw.assign(5 * NS_PER_SEC);
        assert_eq!(w1.start, 0);
        assert_eq!(w1.end, 10 * NS_PER_SEC);

        let w2 = tw.assign(15 * NS_PER_SEC);
        assert_eq!(w2.start, 10 * NS_PER_SEC);
        assert_eq!(w2.end, 20 * NS_PER_SEC);
    }

    #[test]
    fn tumbling_window_boundary() {
        let tw = TumblingWindows::from_secs(10);

        // Exactly on boundary goes to next window
        let w = tw.assign(10 * NS_PER_SEC);
        assert_eq!(w.start, 10 * NS_PER_SEC);
    }

    #[test]
    fn tumbling_window_same_window() {
        let tw = TumblingWindows::from_secs(60);

        let w1 = tw.assign(5 * NS_PER_SEC);
        let w2 = tw.assign(30 * NS_PER_SEC);
        let w3 = tw.assign(59 * NS_PER_SEC);

        assert_eq!(w1, w2);
        assert_eq!(w2, w3);
    }

    #[test]
    fn watermark_advance() {
        let mut wm = Watermark::new(5 * NS_PER_SEC);
        assert_eq!(wm.value(), i64::MIN);

        wm.advance(100 * NS_PER_SEC);
        assert_eq!(wm.value(), 95 * NS_PER_SEC);

        // Earlier event doesn't regress watermark
        wm.advance(90 * NS_PER_SEC);
        assert_eq!(wm.value(), 95 * NS_PER_SEC);

        // Later event advances watermark
        wm.advance(110 * NS_PER_SEC);
        assert_eq!(wm.value(), 105 * NS_PER_SEC);
    }

    #[test]
    fn watermark_late_detection() {
        let mut wm = Watermark::new(5 * NS_PER_SEC);
        wm.advance(100 * NS_PER_SEC); // watermark = 95s

        assert!(wm.is_late(90 * NS_PER_SEC)); // before watermark
        assert!(!wm.is_late(95 * NS_PER_SEC)); // at watermark
        assert!(!wm.is_late(100 * NS_PER_SEC)); // after watermark
    }

    #[test]
    fn session_window_open_extend_close() {
        let config = SessionWindows::from_secs(30);
        let mut tracker = SessionTracker::new(config);

        // First event opens a session
        assert_eq!(
            tracker.on_event(b"user1", 100 * NS_PER_SEC),
            SessionEvent::Opened
        );
        assert_eq!(tracker.active_count(), 1);

        // Event within gap extends it
        assert_eq!(
            tracker.on_event(b"user1", 120 * NS_PER_SEC),
            SessionEvent::Extended
        );

        // Event beyond gap closes old session and opens new
        let result = tracker.on_event(b"user1", 200 * NS_PER_SEC);
        match result {
            SessionEvent::ClosedAndOpened(closed) => {
                assert_eq!(closed.start, 100 * NS_PER_SEC);
                assert_eq!(closed.end, 120 * NS_PER_SEC + 30 * NS_PER_SEC);
            }
            _ => panic!("expected ClosedAndOpened"),
        }
    }

    #[test]
    fn session_window_multiple_keys() {
        let config = SessionWindows::from_secs(10);
        let mut tracker = SessionTracker::new(config);

        assert_eq!(
            tracker.on_event(b"a", 100 * NS_PER_SEC),
            SessionEvent::Opened
        );
        assert_eq!(
            tracker.on_event(b"b", 105 * NS_PER_SEC),
            SessionEvent::Opened
        );
        assert_eq!(tracker.active_count(), 2);

        // Extending key "a" doesn't affect key "b"
        assert_eq!(
            tracker.on_event(b"a", 108 * NS_PER_SEC),
            SessionEvent::Extended
        );
    }

    #[test]
    fn session_close_expired() {
        let config = SessionWindows::from_secs(10);
        let mut tracker = SessionTracker::new(config);

        tracker.on_event(b"early", 100 * NS_PER_SEC);
        tracker.on_event(b"late", 200 * NS_PER_SEC);

        // Watermark at 200s — "early" session (last event at 100s, gap 10s) should close
        let closed = tracker.close_expired(200 * NS_PER_SEC);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].0, b"early");
        assert_eq!(tracker.active_count(), 1);
    }

    #[test]
    fn sliding_window_assignment() {
        let sw = SlidingWindows::from_millis(10_000, 5_000); // 10s window, 5s slide

        let ts = 12 * NS_PER_SEC;
        let windows = sw.assign(ts);

        // Should be in windows [10s, 20s) and [5s, 15s) — but not [15s, 25s) since 12 < 15
        // Actually: last_start = 12 - (12 % 5) = 10
        // Window [10, 20): 10+10 > 12 ✓
        // Window [5, 15): 5+10 > 12 ✓
        // Window [0, 10): 0+10 > 12? No, 10 > 12 is false
        assert_eq!(windows.len(), 2);
        assert!(windows.iter().any(|w| w.start == 10 * NS_PER_SEC));
        assert!(windows.iter().any(|w| w.start == 5 * NS_PER_SEC));
    }

    #[test]
    fn sliding_equals_tumbling_when_slide_equals_size() {
        let tw = TumblingWindows::from_secs(10);
        let sw = SlidingWindows::from_millis(10_000, 10_000);

        let ts = 25 * NS_PER_SEC;
        let tw_window = tw.assign(ts);
        let sw_windows = sw.assign(ts);

        assert_eq!(sw_windows.len(), 1);
        assert_eq!(sw_windows[0], tw_window);
    }

    #[test]
    fn late_policy_variants() {
        // Just verify the enum variants exist and are distinct
        assert_ne!(LatePolicy::Discard, LatePolicy::SideOutput);
        assert_ne!(LatePolicy::SideOutput, LatePolicy::Recompute);
    }
}
