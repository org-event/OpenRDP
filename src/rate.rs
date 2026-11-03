//! Bitrate that follows the link.
//!
//! The frame rate already follows it: only a few frames are in flight, so a
//! slow client gets fewer of them. The bitrate does not, and a link or decoder
//! slower than `--bitrate` then queues data, which shows as frames
//! acknowledged later and later. So the time from sending a frame to its
//! acknowledgement drives the encoder's average bitrate: cut it when that time
//! grows well past the best seen on this connection, and raise it slowly back
//! to `--bitrate` while it stays low.

use std::time::Duration;

/// How often the bitrate may change.
pub const PERIOD: Duration = Duration::from_secs(1);
/// Fewer acknowledgements than this in a period say nothing: a still screen.
const MIN_SAMPLES: u32 = 5;
/// Never below this share of `--bitrate`, nor below 1 Mbit/s: a client that
/// decodes slowly acknowledges late on any link, and should not lose more.
const FLOOR_SHARE: u32 = 4;
const FLOOR: u32 = 1_000_000;
/// Congested when acknowledgements take this much longer than the best seen.
const SLACK: f64 = 2.0;
const SLACK_MS: f64 = 30.0;
/// Calm periods before each raise.
const CALM: u32 = 3;

#[derive(Debug)]
pub struct RateControl {
    max: u32,
    current: u32,
    /// Lowest acknowledgement time of a period on this connection, in ms.
    baseline_ms: Option<f64>,
    calm: u32,
}

impl RateControl {
    pub fn new(max: u32) -> Self {
        Self {
            max,
            current: max,
            baseline_ms: None,
            calm: 0,
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    fn floor(&self) -> u32 {
        (self.max / FLOOR_SHARE).max(FLOOR).min(self.max)
    }

    /// Takes one period's average acknowledgement time over `samples` frames;
    /// returns the new bitrate when it changes.
    pub fn update(&mut self, average_ms: f64, samples: u32) -> Option<u32> {
        if samples < MIN_SAMPLES {
            return None;
        }
        let baseline = self
            .baseline_ms
            .map_or(average_ms, |baseline| baseline.min(average_ms));
        self.baseline_ms = Some(baseline);
        let before = self.current;
        if average_ms > baseline * SLACK + SLACK_MS {
            self.calm = 0;
            self.current = (u64::from(self.current) * 3 / 4) as u32;
            self.current = self.current.max(self.floor());
        } else {
            self.calm += 1;
            if self.calm >= CALM {
                self.calm = 0;
                self.current = (u64::from(self.current) * 23 / 20).min(u64::from(self.max)) as u32;
            }
        }
        (self.current != before).then_some(self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_link_keeps_the_maximum() {
        let mut rate = RateControl::new(20_000_000);
        for _ in 0..20 {
            assert_eq!(rate.update(20.0, 30), None);
        }
        assert_eq!(rate.current(), 20_000_000);
    }

    #[test]
    fn growing_acks_cut_then_calm_raises_back() {
        let mut rate = RateControl::new(20_000_000);
        rate.update(20.0, 30);
        assert_eq!(rate.update(200.0, 30), Some(15_000_000));
        assert_eq!(rate.update(200.0, 30), Some(11_250_000));
        // Three calm periods per raise, never past the maximum.
        let mut periods = 0;
        while rate.current() < 20_000_000 {
            rate.update(25.0, 30);
            periods += 1;
            assert!(periods < 100);
        }
        assert_eq!(rate.current(), 20_000_000);
        assert!(periods >= 3 * 4, "{periods}");
    }

    #[test]
    fn floor_and_still_screens() {
        let mut rate = RateControl::new(20_000_000);
        rate.update(20.0, 30);
        for _ in 0..50 {
            rate.update(500.0, 30);
        }
        assert_eq!(rate.current(), 5_000_000);
        // A still screen sends a frame now and then; its timing proves nothing.
        assert_eq!(rate.update(20.0, 2), None);
        assert_eq!(RateControl::new(2_000_000).floor(), 1_000_000);
    }
}
