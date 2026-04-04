use tokio::time::{interval, Duration};
use std::sync::{Arc, RwLock};
use crate::types::{RustStatus, AppState, GLOBAL_TX};

pub fn spawn_radio_heartbeat(state: Arc<RwLock<AppState>>) {
    tokio::spawn(async move {
        let mut hb_interval = interval(Duration::from_secs(10));
        println!("[任务0] 心跳任务启动，频率: 10s/次");

        loop {
            hb_interval.tick().await;
            
            let target_ip = {
                let s = state.read().unwrap();
                s.radio_ip.clone()
            };

            if let Some(ip) = target_ip {
                if let Err(e) = crate::radio_ctrl::query_status(&ip) {
                    crate::utils::log_to_pc(&format!("⚠️ 心跳包下发失败: {}", e));
                }
            }
        }
    });
}

pub fn spawn_status_heartbeat(state: Arc<RwLock<AppState>>) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(1));
        loop {
            ticker.tick().await;
            let mut pkt = vec![0x11];
            {
                let mut s = state.write().unwrap();
                s.status.uptime_ms = s.start_time.elapsed().as_millis() as u32;
                s.status.timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                let ptr = &s.status as *const RustStatus as *const u8;
                pkt.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(ptr, std::mem::size_of::<RustStatus>())
                });
            }
            if let Some(tx) = GLOBAL_TX.get() {
                let _ = tx.send(pkt);
            }
        }
    });
}
