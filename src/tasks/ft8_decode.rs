use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::Ordering;
use chrono::{Utc, Timelike};
use tokio::time::{interval, Duration};

use crate::types::{
    AppState, DecodeBuffer, WSDecodePkt, Ft8DecodeResult, 
    GLOBAL_TX, AUTO_MGR, CURRENT_DECODE_TS, CURRENT_DT_OFFSET_MS, RADIO_LO_FREQ
};
use crate::config;
use crate::ft8_qso::qso_auto_once::handle_auto_reply_logic;
use crate::ft8_codec::decode_ft8_block;
use crate::ft8_qso::psk_reporter::PskSpot;

/// 启动 FT8 解码调度任务
/// 该任务以 100ms 为周期检查系统时间，在每个 FT8 15s 窗口结束前（第 14.5s 以后）触发解码。
pub fn spawn_decode_task(state: Arc<RwLock<AppState>>, decode_buf: Arc<Mutex<DecodeBuffer>>, psk_tx: tokio::sync::mpsc::Sender<PskSpot>) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(100));
        let mut last_window = -1i32;
        loop {
            ticker.tick().await;
            let now = Utc::now();
            let (window_idx, decode_on) = {
                let s = state.read().unwrap();
                ((now.second() / 15) as i32, s.status.ft8_decode_on == 1)
            };

            // 每 15s 窗口的第 14 秒中后期 (500ms 后) 且 ft8_decode 开关开启时，启动解码
            if decode_on && now.second() % 15 == 14 && now.timestamp_subsec_millis() > 500 && window_idx != last_window {
                last_window = window_idx;
                let aligned_sec = (now.second() / 15) * 15;
                
                // 1. 生成基于窗口对齐的时间戳 (HHMMSS)
                let ts = now.hour() * 10000 + now.minute() * 100 + aligned_sec as u32;
                CURRENT_DECODE_TS.store(ts as u64, Ordering::SeqCst);

                // 2. 计算 DT 偏差偏移 (Time Alignment)
                // 旨在将采集缓冲区的起始点映射到 FT8 标准窗口的 0.0s
                let current_total_ms = (now.second() % 60 * 1000 + now.timestamp_subsec_millis()) as i32;
                let window_start_ms = (aligned_sec * 1000) as i32;
                let buffer_start_ms = current_total_ms - 16000; // 因为我们拉取的是最后 16s 的数据
                let dt_offset = buffer_start_ms - window_start_ms;
                CURRENT_DT_OFFSET_MS.store(dt_offset, Ordering::SeqCst);

                // 3. 统计解码次数并从缓冲区提取音频数据
                state.write().unwrap().status.decode_count += 1;
                let samples = decode_buf.lock().unwrap().get_last_16s();
                let p_tx = psk_tx.clone();

                // 4. 在阻塞线程池中启动 CPU 密集型的解码任务
                tokio::task::spawn_blocking(move || {
                    process_ft8_decode(samples, p_tx);
                });
            }
        }
    });
}

/// 实际执行 FT8 解码及结果处理 (运行在 blocking thread 中)
fn process_ft8_decode(samples_i16: Vec<i16>, psk_tx: tokio::sync::mpsc::Sender<PskSpot>) {
    let ts = CURRENT_DECODE_TS.load(Ordering::SeqCst);
    let dt_offset_ms = CURRENT_DT_OFFSET_MS.load(Ordering::SeqCst);
    let offset_s = dt_offset_ms as f32 / 1000.0;

    let (tx, rx) = std::sync::mpsc::channel::<Ft8DecodeResult>();
    let f32_samples: Vec<f32> = samples_i16.iter().map(|&s| s as f32).collect();

    // 在当前线程作用域内控制解码生命周期
    std::thread::scope(|s| {
        // 调用物理层库执行 FT8 解码
        s.spawn(|| { decode_ft8_block(&f32_samples, tx); });

        // 接收并处理解码出的每一条结果
        while let Ok(res) = rx.recv() {
            // 保存呼号哈希用于后续解析
            for word in res.text.split_whitespace() { crate::ft8_codec::save_hash_call(word); }
            
            // 修正 DT (基于我们计算的偏移)
            let corrected_dt = res.dt + offset_s;

            // 构造 WebSocket 发送格式的数据包
            let mut msg_bytes = [0u8; 32];
            let bytes = res.text.as_bytes();
            let len = bytes.len().min(32);
            msg_bytes[..len].copy_from_slice(&bytes[..len]);

            let pkt = WSDecodePkt {
                hour: (ts / 10000) as u8,
                minute: ((ts % 10000) / 100) as u8,
                second: (ts % 100) as u8,
                snr: res.snr as i8,
                dt: (corrected_dt * 10.0) as i16,
                freq: res.freq as i16,
                message: msg_bytes,
            };

            let p_hour = pkt.hour;
            let p_min = pkt.minute;
            let p_sec = pkt.second;
            let p_snr = pkt.snr;
            let p_freq = pkt.freq;

            // 控制台打印实时解码日志
            println!(
                "[{:02}:{:02}:{:02}] Freq: {:4} Hz | SNR: {:+3} | DT: {:+4.1} | Msg: {} ({}ms)",
                p_hour, p_min, p_sec, p_freq, p_snr, corrected_dt, res.text, res.decode_time_ms
            );

            // --- A. 通过 WebSocket 广播解码结果到所有在线客户端 ---
            if let Some(global_tx) = GLOBAL_TX.get() {
                let mut ws_msg = vec![0x03]; // 0x03 类型: FT8 解码结果回传
                let ptr = &pkt as *const WSDecodePkt as *const u8;
                ws_msg.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, std::mem::size_of::<WSDecodePkt>()) });
                let _ = global_tx.send(ws_msg);
            }

            // --- B. 执行自动答复逻辑 (针对 Mode 2: 手动选择目标后的自动通联) ---
            handle_auto_reply_logic(&res.text, res.snr);

            // --- C. 进入 AutoQsoManager 状态机处理 (针对 Mode 3: 自动化全通联处理) ---
            {
                let mut mgr = AUTO_MGR.get().unwrap().lock().unwrap();
                mgr.push_decode(res.clone());       // 更新历史解码信息
                mgr.check_and_log_qso(&res);        // 检查通联是否完成
                if res.text.contains(config::MY_CALL) { mgr.report_any_reply(); } // 用于防止误触发紧急 CQ
            }

            // --- D. 若设置了网格，则上报至 PSK Reporter 服务 ---
            let psk_spot = PskSpot {
                callsign: res.sender_call.unwrap_or_default(),
                frequency_hz: (RADIO_LO_FREQ.load(Ordering::SeqCst) as f32 + res.freq) as u64,
                snr: res.snr as i8,
                dt_ms: (corrected_dt * 1000.0) as i16,
                sender_grid: res.grid.unwrap_or_default(),
            };
            if !psk_spot.callsign.is_empty() && psk_spot.callsign != "..." {
                 let _ = psk_tx.try_send(psk_spot);
            }
        }
    });
}
