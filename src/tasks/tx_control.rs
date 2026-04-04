use tokio::time::{interval, Duration};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::ffi::CString;
use chrono::{Utc, Timelike};

use crate::types::{AppState, STATE, AUTO_MGR};
use crate::utils::log_to_pc;
use crate::ft8_codec::encode_ft8_symbols;
use crate::config;
use crate::ft8_qso::qso_auto_hunter::AutoQsoManager;

pub fn spawn_tx_check_task(state: Arc<RwLock<AppState>>) {
    let udp_tx = Arc::new(std::net::UdpSocket::bind("0.0.0.0:0").unwrap());
    let target_radio_addr: SocketAddr = config::RADIO_ADDR.parse().unwrap();

    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(10));
        loop {
            ticker.tick().await;
            let now = Utc::now();
            let sec = now.second();

            if (sec == 0 || sec == 15 || sec == 30 || sec == 45) && now.timestamp_subsec_millis() < 900 {
                let tx_info = {
                    let s = state.read().unwrap();
                    let is_even_win = sec == 0 || sec == 30;
                    if s.status.auto_tx_mode > 0 && (s.status.tx_window_even == 1) == is_even_win {
                        let msg = String::from_utf8_lossy(&s.status.pending_msg).trim_matches(char::from(0)).to_string();
                        if !msg.is_empty() {
                            let use_f = if s.status.auto_tx_mode == 3 && s.status.repeat_count >= 2 && !msg.starts_with("CQ ") {
                                s.target_offset
                            } else {
                                s.status.pending_offset
                            };
                            Some((msg, use_f, s.status.auto_tx_mode, s.status.max_repeats))
                        } else { None }
                    } else { None }
                };

                if let Some((msg, offset, mode, max_rep)) = tx_info {
                    let mut tones = [0i32; 79];
                    let c_msg = CString::new(msg.clone()).unwrap();
                    unsafe { encode_ft8_symbols(c_msg.as_ptr(), tones.as_mut_ptr()); }

                    let mut pkt = Vec::with_capacity(109);
                    pkt.extend_from_slice(&0x2DE3u16.to_le_bytes()); // MAGIC
                    pkt.push(0x01); // VER
                    pkt.push(28); // Type: TX Data
                    pkt.extend_from_slice(&offset.to_le_bytes());

                    for t in tones { pkt.push(t as u8); }
                    let msg_bytes = msg.as_bytes();
                    let text_len = msg_bytes.len().min(24);
                    pkt.extend_from_slice(&msg_bytes[..text_len]);

                    for word in msg.split_whitespace() { crate::ft8_codec::save_hash_call(word); }
                    let _ = udp_tx.send_to(&pkt, target_radio_addr);

                    {
                        let mut s = state.write().unwrap();
                        s.status.tx_count += 1;
                        if s.status.pending_msg == s.status.last_msg { s.status.repeat_count += 1; }
                        else { 
                            s.status.repeat_count = 1; 
                            s.status.last_msg = s.status.pending_msg;
                        }
                        if mode == 1 || (mode == 2 && (s.status.repeat_count >= max_rep || msg.contains(" 73"))) {
                            s.status.auto_tx_mode = 0;
                        }
                    }
                    log_to_pc(&format!("🚀 已下发发射命令: {} (带回显文本)", msg));
                }
                tokio::time::sleep(Duration::from_millis(910)).await;
            }
        }
    });
}

pub fn spawn_auto_qso_timer_task() {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(100));
        loop {
            ticker.tick().await;
            let (mode, is_idle, repeat_count, current_msg) = {
                let s = STATE.get().unwrap().read().unwrap();
                let msg = String::from_utf8_lossy(&s.status.pending_msg).trim_matches(char::from(0)).to_string();
                (s.status.auto_tx_mode, s.status.pending_msg[0] == 0, s.status.repeat_count, msg)
            };
            if mode != 3 { continue; }

            let now = Utc::now();
            let sec = now.second();
            let ms = now.timestamp_subsec_millis();
            let mut should_sleep = false;

            {
                let mut mgr = AUTO_MGR.get().unwrap().lock().unwrap();
                let mgr: &mut AutoQsoManager = &mut *mgr;
                let is_73 = current_msg.contains(" 73") || current_msg.contains(" RR73");
                let is_cq = current_msg.starts_with("CQ ");
                let is_chase = !is_73 && !is_cq;
                let limit_reached = if is_73 { repeat_count >= 1 } else if is_chase { repeat_count >= 3 } else { repeat_count >= 4 };

                if !is_idle && limit_reached {
                    if is_chase { mgr.report_failure(); }
                    let state_arc = STATE.get().unwrap();
                    let mut s = state_arc.write().unwrap();
                    if mgr.consecutive_failures < 2 {
                        if let Some((next_m, next_f, target_f, next_e)) = mgr.task_queue.pop_front() {
                            let next_m: String = next_m;
                            let bytes = next_m.as_bytes();
                            s.status.pending_msg = [0u8; 24];
                            let len = bytes.len().min(24);
                            s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
                            s.status.pending_offset = next_f as u16;
                            s.target_offset = target_f as u16;
                            s.status.tx_window_even = if next_e { 1 } else { 0 };
                            s.status.repeat_count = 0;
                            log_to_pc(&format!("⏭️ 自动切换下一条: {}", next_m));
                        } else {
                            s.status.pending_msg = [0u8; 24];
                            log_to_pc("⏭️ 已达发送上限，清空待发射消息");
                        }
                    } else {
                        s.status.pending_msg = [0u8; 24];
                        log_to_pc("⏭️ 已达连续失败阈值，清空当前任务，准备触发紧急 CQ");
                    }
                }

                if (sec == 14 || sec == 29 || sec == 44 || sec == 59) && ms >= 900 {
                    let current_is_idle = STATE.get().unwrap().read().unwrap().status.pending_msg[0] == 0;
                    if let Some((msg, f, e)) = mgr.check_auto_cq(current_is_idle) {
                        let mut s = STATE.get().unwrap().write().unwrap();
                        let msg: String = msg;
                        let bytes = msg.as_bytes();
                        s.status.pending_msg = [0u8; 24];
                        let len = bytes.len().min(24);
                        s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
                        s.status.pending_offset = f as u16;
                        s.target_offset = f as u16;
                        s.status.tx_window_even = if e { 1 } else { 0 };
                        s.status.repeat_count = 0;
                        log_to_pc(&format!("🎯 策略触发: {}", msg));
                    } else if let Some((msg, f, target_f, e)) = mgr.check_auto_chase(is_idle) {
                        let mut s = STATE.get().unwrap().write().unwrap();
                        let msg: String = msg;
                        let bytes = msg.as_bytes();
                        s.status.pending_msg = [0u8; 24];
                        let len = bytes.len().min(24);
                        s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
                        s.status.pending_offset = f as u16;
                        s.target_offset = target_f as u16;
                        s.status.tx_window_even = if e { 1 } else { 0 };
                        s.status.repeat_count = 0;
                        log_to_pc(&format!("🎯 策略触发: {}", msg));
                    }
                    should_sleep = true;
                }
            }
            if should_sleep { tokio::time::sleep(Duration::from_millis(150)).await; }
        }
    });
}
