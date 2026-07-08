use std::time::{Duration, Instant};

use tokio::time::sleep;

pub(crate) struct MediaClock {
    started_at: Option<Instant>,
}

impl MediaClock {
    pub(crate) fn new() -> Self {
        Self { started_at: None }
    }

    pub(crate) async fn wait_until(&mut self, pts_us: Option<i64>) {
        let Some(pts_us) = pts_us else {
            return;
        };
        if pts_us <= 0 {
            return;
        }

        let started_at = *self.started_at.get_or_insert_with(Instant::now);
        let target = Duration::from_micros(pts_us as u64);
        if let Some(delay) = target.checked_sub(started_at.elapsed()) {
            sleep(delay).await;
        }
    }
}
