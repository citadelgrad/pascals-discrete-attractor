//! Detects repetitive tool-calling loops and provides steering injection.

use std::collections::VecDeque;

/// Detects when an agent falls into a repetitive tool-calling loop.
pub struct LoopDetector {
    window_size: usize,
    /// Recent tool call signatures: (name, hash_of_args)
    recent_calls: VecDeque<String>,
}

impl LoopDetector {
    pub fn new(window_size: usize) -> Self {
        Self {
            window_size,
            recent_calls: VecDeque::new(),
        }
    }

    /// Record a tool call and check if we're in a loop.
    /// Returns true if a loop is detected.
    pub fn record_and_check(&mut self, tool_name: &str, arguments: &serde_json::Value) -> bool {
        if self.window_size == 0 {
            return false;
        }

        let signature = format!("{}:{}", tool_name, Self::hash_args(arguments));
        self.recent_calls.push_back(signature);

        // Keep window bounded
        while self.recent_calls.len() > self.window_size {
            self.recent_calls.pop_front();
        }

        // Check if all entries in the window are identical
        if self.recent_calls.len() >= self.window_size {
            let first = &self.recent_calls[0];
            self.recent_calls.iter().all(|s| s == first)
        } else {
            false
        }
    }

    /// Reset the detector (e.g., after steering injection).
    pub fn reset(&mut self) {
        self.recent_calls.clear();
    }

    /// Simple hash of arguments for comparison.
    fn hash_args(args: &serde_json::Value) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let s = args.to_string();
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }
}

/// Steering messages that can be injected when issues are detected.
pub struct SteeringInjector;

impl SteeringInjector {
    /// Generate a steering message for a detected loop.
    pub fn loop_detected_message(tool_name: &str, window_size: usize) -> String {
        format!(
            "WARNING: You have called the '{}' tool {} times in a row with the same arguments. \
             This appears to be a loop. Please try a different approach or tool.",
            tool_name, window_size
        )
    }

    /// Generate a steering message to refocus on the task.
    pub fn refocus_message(goal: &str) -> String {
        format!(
            "Please refocus on the current goal: {}. \
             Consider what steps are still needed and take a different approach.",
            goal
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Test 1: No loop detected with varied calls
    #[test]
    fn no_loop_with_varied_calls() {
        let mut detector = LoopDetector::new(3);

        assert!(!detector.record_and_check("read_file", &json!({"path": "/a.rs"})));
        assert!(!detector.record_and_check("write_file", &json!({"path": "/b.rs"})));
        assert!(!detector.record_and_check("exec", &json!({"cmd": "cargo build"})));
        assert!(!detector.record_and_check("grep", &json!({"pattern": "foo"})));
    }

    // Test 2: Loop detected with identical calls filling the window
    #[test]
    fn loop_detected_with_identical_calls() {
        let mut detector = LoopDetector::new(3);

        let args = json!({"path": "/same.rs"});
        assert!(!detector.record_and_check("read_file", &args));
        assert!(!detector.record_and_check("read_file", &args));
        // Third identical call fills the window -> loop detected
        assert!(detector.record_and_check("read_file", &args));
    }

    // Test 3: Reset clears the state
    #[test]
    fn reset_clears_state() {
        let mut detector = LoopDetector::new(3);

        let args = json!({"path": "/same.rs"});
        assert!(!detector.record_and_check("read_file", &args));
        assert!(!detector.record_and_check("read_file", &args));

        detector.reset();

        // After reset, need to fill the window again
        assert!(!detector.record_and_check("read_file", &args));
        assert!(!detector.record_and_check("read_file", &args));
        assert!(detector.record_and_check("read_file", &args));
    }

    // Test 4: Window boundary -- one less than window doesn't trigger
    #[test]
    fn window_boundary_no_trigger() {
        let mut detector = LoopDetector::new(4);

        let args = json!({"x": 1});
        assert!(!detector.record_and_check("tool_a", &args));
        assert!(!detector.record_and_check("tool_a", &args));
        // 3 identical calls with window_size=4 should NOT trigger
        assert!(!detector.record_and_check("tool_a", &args));
        // 4th call fills the window -> triggers
        assert!(detector.record_and_check("tool_a", &args));
    }

    // Test 5: Different args for same tool don't trigger
    #[test]
    fn different_args_same_tool_no_trigger() {
        let mut detector = LoopDetector::new(3);

        assert!(!detector.record_and_check("read_file", &json!({"path": "/a.rs"})));
        assert!(!detector.record_and_check("read_file", &json!({"path": "/b.rs"})));
        assert!(!detector.record_and_check("read_file", &json!({"path": "/c.rs"})));
        assert!(!detector.record_and_check("read_file", &json!({"path": "/d.rs"})));
    }

    // Test 6: Steering injector messages
    #[test]
    fn steering_messages_are_well_formed() {
        let msg = SteeringInjector::loop_detected_message("read_file", 5);
        assert!(msg.contains("read_file"));
        assert!(msg.contains("5 times"));
        assert!(msg.contains("loop"));

        let refocus = SteeringInjector::refocus_message("fix the build error");
        assert!(refocus.contains("fix the build error"));
        assert!(refocus.contains("different approach"));
    }

    // Test 7: Loop broken by a different call in the middle
    #[test]
    fn loop_broken_by_different_call() {
        let mut detector = LoopDetector::new(3);

        let args = json!({"path": "/same.rs"});
        assert!(!detector.record_and_check("read_file", &args));
        assert!(!detector.record_and_check("read_file", &args));
        // Insert a different call
        assert!(!detector.record_and_check("write_file", &json!({"path": "/out.rs"})));
        // Continue with the same call -- window now has [read, write, read], no loop
        assert!(!detector.record_and_check("read_file", &args));
    }

    #[test]
    fn zero_window_never_triggers() {
        let mut detector = LoopDetector::new(0);
        for _ in 0..100 {
            assert!(!detector.record_and_check("read_file", &json!({"path": "/same.rs"})));
        }
    }
}
