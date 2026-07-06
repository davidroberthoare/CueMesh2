//! Periodic SYNC ping loop.

use std::time::Duration;

use cuemesh2_shared::protocol::{ControllerMsg, SyncPing};

use crate::server::{broadcast, now_utc_ms};
use crate::state::SharedState;

pub async fn run(state: SharedState) {
    let mut interval = tokio::time::interval(Duration::from_millis(1000));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut token: u64 = 0;
    loop {
        interval.tick().await;
        token = token.wrapping_add(1);
        broadcast(
            &state,
            ControllerMsg::Sync(SyncPing {
                t1_utc_ms: now_utc_ms(),
                token,
            }),
        );
    }
}
