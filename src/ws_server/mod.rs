use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State},
    response::IntoResponse,
};
use futures_util::{stream::StreamExt, sink::SinkExt};
use std::sync::{Arc, RwLock};
use regex::Regex;
use std::net::SocketAddr;
use tokio::net::UdpSocket;

use crate::types::{AppState, GLOBAL_TX};
use crate::utils::{log_to_pc};
use crate::ft8_qso::qso_utils::get_next_expect_regex;
use crate::config;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RwLock<AppState>>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<RwLock<AppState>>) {
    let (mut sender, mut receiver) = socket.split();
    let mut broadcast_rx = GLOBAL_TX.get().unwrap().subscribe();

    let _state_ws = state.clone();
    let mut t1 = tokio::spawn(async move {
        while let Ok(msg) = broadcast_rx.recv().await {
            if sender.send(Message::Binary(msg.into())).await.is_err() { break; }
        }
    });

    let state_ctrl = state.clone();
    let mut t2 = tokio::spawn(async move {
        let udp_cmd = UdpSocket::bind("0.0.0.0:0").await.unwrap();

        while let Some(Ok(Message::Binary(bin))) = receiver.next().await {
            if bin.is_empty() { continue; }
            match bin[0] {
                0x02 => {
                    // 动态获取电台 IP 并发送原始指令包
                    let radio_ip = {
                        let s = state_ctrl.read().unwrap();
                        s.radio_ip.clone()
                    };
                    
                    if let Some(ip) = radio_ip {
                        if let Ok(target_addr) = format!("{}:{}", ip, crate::radio_ctrl::RADIO_CTRL_PORT).parse::<SocketAddr>() {
                            let _ = udp_cmd.send_to(&bin[1..], target_addr).await;
                        }
                    } else {
                        log_to_pc("⚠️ [WS] 无法发送指令：尚未发现电台 IP");
                    }
                }
                0x10 => {
                    let cmd_type = bin[1];
                    let mut s = state_ctrl.write().unwrap();
                    match cmd_type {
                        1 => {
                            s.status.auto_tx_mode = 0;
                            s.status.repeat_count = 0;
                            s.status.expect_regex = [0; 48];
                            s.expect_regex_compiled = None;
                        }
                        2 => {
                            if bin.len() >= 30 {
                                s.status.auto_tx_mode = bin[2];
                                s.status.tx_window_even = bin[3];
                                s.status.pending_offset = u16::from_le_bytes([bin[4], bin[5]]);
                                if bin[2] != 3 {
                                    s.status.pending_msg.copy_from_slice(&bin[6..30]);
                                } else {
                                    s.status.pending_msg = [0u8; 24];
                                }
                                s.status.repeat_count = 0;

                                let msg_cow = String::from_utf8_lossy(&s.status.pending_msg);
                                let pending_str_clean = msg_cow.trim_matches(char::from(0)).trim();
                                let next_re_str = get_next_expect_regex(pending_str_clean, config::MY_CALL);

                                if !next_re_str.is_empty() {
                                    let re_bytes = next_re_str.as_bytes();
                                    s.status.expect_regex = [0u8; 48];
                                    let re_len = re_bytes.len().min(48);
                                    s.status.expect_regex[..re_len].copy_from_slice(&re_bytes[..re_len]);
                                    match Regex::new(&next_re_str) {
                                        Ok(re) => s.expect_regex_compiled = Some(re),
                                        Err(e) => {
                                            log_to_pc(&format!("正则编译失败: {}", e));
                                            s.expect_regex_compiled = None;
                                        }
                                    }
                                } else {
                                    s.status.expect_regex = [0u8; 48];
                                    s.expect_regex_compiled = None;
                                }
                            }
                        }
                        3 => {
                            s.current_if_hz = u16::from_le_bytes([bin[2], bin[3]]) as u32;
                        }
                        4 => {
                            s.status.max_repeats = bin[2];
                        }
                        5 => {
                            s.status.demod_mode = bin[2];
                            log_to_pc(&format!("📻 设置解调模式: {}", match bin[2] {
                                0 => "USB",
                                1 => "LSB",
                                2 => "AM",
                                _ => "Unknown"
                            }));
                        }
                        6 => {
                            s.status.ft8_decode_on = bin[2];
                            log_to_pc(&format!("FT8 解码功能已{}", if bin[2] == 1 { "开启" } else { "关闭" }));
                        }
                        7 => {
                            s.status.audio_on = bin[2];
                            log_to_pc(&format!("🔊 音频流已{}", if bin[2] == 1 { "开启" } else { "关闭" }));
                        }
                        8 => {
                            if bin.len() >= 8 {
                                let u = u16::from_le_bytes([bin[2], bin[3]]);
                                let l = u16::from_le_bytes([bin[4], bin[5]]);
                                let a = u16::from_le_bytes([bin[6], bin[7]]);
                                s.status.usb_bw = u;
                                s.status.lsb_bw = l;
                                s.status.am_bw = a;
                                log_to_pc(&format!("📐 带宽已更新: USB={}Hz, LSB={}Hz, AM={}Hz", u, l, a));
                            }
                        }
                        9 => {
                            if bin.len() >= 9 {
                                s.status.wf_speed = bin[2];
                                s.status.wf_min_db = i16::from_le_bytes([bin[3], bin[4]]);
                                s.status.wf_max_db = i16::from_le_bytes([bin[5], bin[6]]);
                                s.status.wf_full_on = bin[7];
                                s.status.wf_zoom_on = bin[8];
                                let min_db = s.status.wf_min_db;
                                let max_db = s.status.wf_max_db;
                                log_to_pc(&format!("🌈 瀑布图参数已更新: Speed={}, Range={}~{}dB", bin[2], min_db, max_db));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    });

    tokio::select! { _ = &mut t1 => t2.abort(), _ = &mut t2 => t1.abort() }
}
