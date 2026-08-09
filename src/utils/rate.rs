// Copyright 2026 Tree xie.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Utilities for computing rates by diffing.

use std::time::Duration;

/// Compute a per-second rate from two cumulative counter values and the
/// elapsed time.
///
/// - Returns 0 when the counter wrapped or was reset (`curr < prev`),
///   avoiding absurdly large values.
/// - Returns 0 when the elapsed time is zero, avoiding division by zero.
pub fn rate_per_sec(prev: u64, curr: u64, elapsed: Duration) -> u64 {
    if curr < prev {
        return 0;
    }
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0;
    }
    ((curr - prev) as f64 / secs).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_rate() {
        assert_eq!(rate_per_sec(1000, 3000, Duration::from_secs(2)), 1000);
    }

    #[test]
    fn sub_second_interval() {
        assert_eq!(rate_per_sec(0, 500, Duration::from_millis(500)), 1000);
    }

    #[test]
    fn counter_reset_returns_zero() {
        assert_eq!(rate_per_sec(5000, 100, Duration::from_secs(1)), 0);
    }

    #[test]
    fn zero_elapsed_returns_zero() {
        assert_eq!(rate_per_sec(0, 1000, Duration::ZERO), 0);
    }
}
