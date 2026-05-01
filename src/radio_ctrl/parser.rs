use std::sync::atomic::Ordering;
use crate::types::{RADIO_LO_FREQ, Ft8DecodeResult, AUTO_MGR};
use crate::utils::log_to_pc;
use crate::radio_ctrl::*;

/// 从 data 的指定偏移读取小端 u16
#[inline] fn r_u16(data: &[u8], off: usize) -> u16 { u16::from_le_bytes([data[off], data[off+1]]) }
/// 从 data 的指定偏移读取小端 u64
#[inline] fn r_u64(data: &[u8], off: usize) -> u64 { u64::from_le_bytes(data[off..off+8].try_into().unwrap()) }

/// 解析电台专有协议的数据包
pub fn parse_radio_packet(data: &[u8]) {
    if data.len() < 4 { return; }

    // 校验 Magic 0x2DE3 (小端序列 [0xE3, 0x2D])
    if data[0] != 0xE3 || data[1] != 0x2D { 
        return; 
    }

    let msg_type = data[3];

    match msg_type {
        // --- Type 4: RadioStatus (电台状态同步) ---
        // Payload 起始于 data[4]，共计 73 字节，完整包长 >= 77
        4 => {
            if data.len() < 77 { return; }
            let p = &data[4..]; // payload 起点

            // ===== 系统信息区 (offset 0~32) =====
            let uptime_us       = r_u64(p, 0);
            let timestamp_us    = r_u64(p, 8);
            let core_temp       = r_u16(p, 18);
            let wifi_rssi       = p[20] as i8;
            let core0_idle      = p[25];
            let core1_idle      = p[26];
            let ram_usage       = p[27];
            let psram_usage     = p[28];

            // ===== 射频参数区 (offset 33~56) =====
            let lo_freq         = r_u64(p, 33);
            let tx_base_freq    = r_u64(p, 41);
            let tx_offset       = r_u16(p, 49);
            let tx_state        = p[51];
            let rx_state        = p[52];
            let bpf_select      = p[53];
            let lpf_select      = p[54];

            // ===== 功放区 (offset 57~72) =====
            let pa_current      = r_u16(p, 61);
            let swr             = r_u16(p, 67);

            // --- 更新全局 LO 频率 (每包都更新，不受节流) ---
            if lo_freq > 0 {
                RADIO_LO_FREQ.store(lo_freq / 100, Ordering::SeqCst);
            }

            // --- 节流：最多 4 秒打印一次 ---
            use std::sync::atomic::AtomicU64;
            static LAST_PRINT_MS: AtomicU64 = AtomicU64::new(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let last = LAST_PRINT_MS.load(Ordering::Relaxed);
            if now_ms - last < 4000 { return; }
            LAST_PRINT_MS.store(now_ms, Ordering::Relaxed);

            let drift_ms = (now_ms as i64 * 1000 - timestamp_us as i64) / 1000;

            println!(
                "[📻] Δt:{:+}ms | {:.0}s | {}℃ | WiFi:{}dB | CPU:{}%/{}% | RAM:{}% PS:{}% | LO:{:.3}M TX:{:.3}M+{}Hz | T:{} R:{} BPF:{} LPF:{} | I:{}mA SWR:{:.2}",
                drift_ms,
                uptime_us as f64 / 1e6,
                core_temp,
                wifi_rssi,
                100 - core0_idle, 100 - core1_idle,
                ram_usage, psram_usage,
                lo_freq as f64 / 1e8,
                tx_base_freq as f64 / 1e8,
                tx_offset,
                tx_state, rx_state, bpf_select, lpf_select,
                pa_current,
                swr as f64 / 100.0,
            );
        }

        // --- Type 5: 日志打印消息 (Active) ---
        CMD_LOG => {
            let text = String::from_utf8_lossy(&data[4..])
                .trim_matches(char::from(0))
                .trim()
                .to_string();
            if !text.is_empty() {
                log_to_pc(&format!("📻 [电台日志]: {}", text));
            }
        }

        // --- Type 50: 指令执行结果 (Response/Active) ---
        MSG_RESULT => {
            if data.len() >= 6 {
                let orig_cmd = data[4];
                let result_code = data[5];
                let text = String::from_utf8_lossy(&data[6..])
                    .trim_matches(char::from(0))
                    .trim()
                    .to_string();
                
                let status_str = if result_code == 0 { "✅ 成功" } else { "❌ 失败" };
                log_to_pc(&format!("🔔 [执行结果] 指令: {} | 结果: {} | 消息: {}", orig_cmd, status_str, text));
            }
        }

        // --- Type 51: FT8 发射成功回传 (发射回显 ECHO) ---
        MSG_FT8_ECHO => {
            if data.len() >= 22 {
                let echo_offset = u16::from_le_bytes([data[20], data[21]]);
                let text = String::from_utf8_lossy(&data[22..])
                    .trim_matches(char::from(0))
                    .trim()
                    .to_string();

                if text.is_empty() { return; }

                log_to_pc(&format!("📡 [发射确认] Freq: {} Hz | Msg: {}", echo_offset, text));

                // A. 记录涉及我呼号的消息至本地文件
                if text.contains(crate::config::MY_CALL) {
                    crate::utils::log_qso_activity(true, &text);
                }

                // B. 注入本地历史池，驱动自动通联状态机
                let parts: Vec<&str> = text.split_whitespace().collect();
                let (receiver, sender) = if parts.len() >= 2 {
                    (Some(parts[0].to_string()), Some(parts[1].to_string()))
                } else {
                    (None, None)
                };

                // 将本地发射的呼号（包括本机呼号）也注入到哈希表，防止别人发来哈希时无法解析
                for part in &parts {
                    crate::ft8_codec::save_hash_call(part);
                }

                let echo_res = Ft8DecodeResult {
                    freq: echo_offset as f32,
                    dt: 0.0,
                    snr: 99, 
                    text: text.clone(),
                    decode_time_ms: 0,
                    sender_call: sender,
                    receiver_call: receiver,
                    grid: None,
                    region: None,
                };

                if let Some(mgr_arc) = AUTO_MGR.get() {
                    if let Ok(mut mgr) = mgr_arc.lock() {
                        use chrono::Timelike;
                        let is_even = (chrono::Utc::now().second() / 15) % 2 == 0;
                        mgr.push_decode(echo_res, is_even);
                    }
                }
            }
        }

        _ => {
            // println!("❓ 收到未知类型电台包: {}", msg_type);
        }
    }
}
