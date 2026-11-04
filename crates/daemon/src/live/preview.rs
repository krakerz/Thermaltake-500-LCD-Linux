//! Throttled preview encoding: the JPEG is made only when a GUI asks, at most a few times a
//! second, and never twice for the same captured frame.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tt500_lcd_ipc::Size;

/// Shortest time between two preview encodes (about 4 per second).
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// One encoded preview.
#[derive(Debug, Clone)]
pub struct Preview {
    /// The JPEG bytes.
    pub jpeg: Arc<Vec<u8>>,
    /// Size of the full source frame (not of the possibly downscaled JPEG).
    pub size: Size,
    /// Capture counter of the frame the JPEG shows.
    pub seq: u64,
}

/// The last preview and when it was encoded.
#[derive(Debug, Default)]
pub struct PreviewCache {
    last: Option<(Instant, Preview)>,
}

impl PreviewCache {
    /// The cached preview when it is still good for frame `seq` at `now`: either it shows
    /// that very frame or it is younger than [`MIN_INTERVAL`].
    #[must_use]
    pub fn fresh(&self, now: Instant, seq: u64) -> Option<Preview> {
        let (at, preview) = self.last.as_ref()?;
        let young = now.saturating_duration_since(*at) < MIN_INTERVAL;
        (preview.seq == seq || young).then(|| preview.clone())
    }

    /// Remembers a preview encoded at `now`.
    pub fn store(&mut self, now: Instant, preview: Preview) {
        self.last = Some((now, preview));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(seq: u64) -> Preview {
        Preview {
            jpeg: Arc::new(vec![1, 2, 3]),
            size: Size {
                width: 2,
                height: 2,
            },
            seq,
        }
    }

    #[test]
    fn empty_cache_is_never_fresh() {
        assert!(PreviewCache::default().fresh(Instant::now(), 1).is_none());
    }

    #[test]
    fn reuses_within_the_interval_even_for_a_newer_frame() {
        let t0 = Instant::now();
        let mut cache = PreviewCache::default();
        cache.store(t0, preview(1));
        let hit = cache.fresh(t0 + MIN_INTERVAL / 2, 5).expect("young");
        assert_eq!(hit.seq, 1);
    }

    #[test]
    fn re_encodes_after_the_interval_when_the_frame_changed() {
        let t0 = Instant::now();
        let mut cache = PreviewCache::default();
        cache.store(t0, preview(1));
        assert!(cache.fresh(t0 + MIN_INTERVAL, 2).is_none());
    }

    #[test]
    fn same_frame_is_reused_for_ever() {
        let t0 = Instant::now();
        let mut cache = PreviewCache::default();
        cache.store(t0, preview(7));
        assert!(cache.fresh(t0 + Duration::from_secs(60), 7).is_some());
    }
}
