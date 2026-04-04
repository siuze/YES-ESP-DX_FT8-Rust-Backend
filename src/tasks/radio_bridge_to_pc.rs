use tokio::net::UdpSocket;
use std::sync::atomic::Ordering;
use crate::types::{GLOBAL_TX, RADIO_LO_FREQ, Ft8DecodeResult, AUTO_MGR};
use crate::ft8_qso::qso_auto_hunter::AutoQsoManager;

/// 启动无线电数据桥接任务
/// 监听电台发出的 UDP 数据包 (默认端口 1532)，转发给前端并执行关键参数解析。
pub fn spawn_radio_bridge_to_pc_task() {
    tokio::spawn(async move {
        let socket = UdpSocket::bind("0.0.0.0:1532").await.unwrap();
        let mut buf = [0u8; 2048];
        loop {
            if let Ok((n, _)) = socket.recv_from(&mut buf).await {
                // 1. 将原始二进制数据包封装并广播至所有 WebSocket 客户端 (前端用于协议调试或显示)
                let mut p = vec![0x01]; // 类型 0x01: 原始 Radio 协议透传
                p.extend_from_slice(&buf[..n]);
                if let Some(tx) = GLOBAL_TX.get() {
                    let _ = tx.send(p);
                }

                // 2. 解析电台发回的关键状态信息
                extract_radio_info(&buf[..n]);
            }
        }
    });
}

/// 解析电台专有协议的数据包
pub fn extract_radio_info(data: &[u8]) {
    if data.len() < 4 { return; }

    // 校验 Magic 0x2DE3 (小端序列 [0xE3, 0x2D])
    if data[0] != 0xE3 || data[1] != 0x2D { 
        return; 
    }

    // --- Type 4: RadioStatus (电台状态同步) ---
    if data[3] == 4 && data.len() >= 45 {
        // 频率信息通常位于偏移 37 字节处 (8字节)
        let lo_bytes: [u8; 8] = data[37..45].try_into().unwrap();
        let lo_freq = u64::from_le_bytes(lo_bytes);
        
        // 更新全局本振频率 (LO)，用于将偏移量换算为绝对频率
        if lo_freq > 0 {
            // 注意：某些固件版本可能需要进行常数除法调整 (如 /100) 以匹配标准的 Hz
            RADIO_LO_FREQ.store(lo_freq/100, Ordering::SeqCst);
        }
    }

    // --- Type 51: FT8 发射成功回传 (发射回显 ECHO) ---
    // 当电台物理层成功发射出一帧 FT8 信号后，会通过此包回传发射文本和频率
    else if data[3] == 51 && data.len() >= 22 {
        let echo_offset = u16::from_le_bytes([data[20], data[21]]);
        let text = String::from_utf8_lossy(&data[22..])
            .trim_matches(char::from(0))
            .trim()
            .to_string();

        if text.is_empty() { return; }

        // 记录发射消息至日志文件
        if text.contains(crate::config::MY_CALL) {
            crate::utils::log_qso_activity(true, &text);
        }

        let parts: Vec<&str> = text.split_whitespace().collect();
        let (receiver, sender) = if parts.len() >= 2 {
            (Some(parts[0].to_string()), Some(parts[1].to_string()))
        } else {
            (None, None)
        };

        // 构造一个 SNR=99 的特殊解码点，注入本地历史池
        // 这样自动通联管理器就能知道“我刚才发了什么”，从而推进通联状态机
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
                let mgr: &mut AutoQsoManager = &mut *mgr;
                mgr.push_decode(echo_res); // 写入历史，用于连贯性判定
            }
        }
    }
}
