use std::sync::atomic::Ordering;
use crate::types::{RADIO_LO_FREQ, Ft8DecodeResult, AUTO_MGR};
use crate::utils::log_to_pc;
use crate::radio_ctrl::*;

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
        4 => {
            if data.len() >= 45 {
                // 频率信息位于偏移 37 字节处 (8字节)
                let lo_bytes: [u8; 8] = data[37..45].try_into().unwrap_or([0; 8]);
                let lo_freq = u64::from_le_bytes(lo_bytes);
                
                // 更新全局本振频率 (LO)
                if lo_freq > 0 {
                    // 同步到原子变量，供 FT8 解码换算绝对频率
                    RADIO_LO_FREQ.store(lo_freq / 100, Ordering::SeqCst);
                }
            }
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
                        mgr.push_decode(echo_res);
                    }
                }
            }
        }

        _ => {
            // println!("❓ 收到未知类型电台包: {}", msg_type);
        }
    }
}
