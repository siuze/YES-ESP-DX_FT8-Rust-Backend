// ----------------------------------------------------------------------------
// FT8 SDR 后端管理系统 (Refactored V2)
// ----------------------------------------------------------------------------
// 功能：集成 FT8 自动通联 (Mode 3)、多模式解调 (USB/LSB/AM)、音频压缩流、Notion 日志上报等。
// 本文件负责项目总入口：初始化全局状态、挂载子模块并启动所有后台异步任务。
// ----------------------------------------------------------------------------

pub mod config;       // 个人配置与敏感信息（呼号、网格、API凭证等）
pub mod types;        // 核心数据结构与全局静态状态
pub mod utils;        // 基础工具类（日志输出、字节解析等）
pub mod ft8_codec;    // FT8 物理层编解码封装 (原 ft8_rust)
pub mod ft8_qso;      // 自动通联业务大类 (包含 auto_qso, notion_logger, psk_reporter)
pub mod tasks;        // 具体的后台常驻异步任务
pub mod dsp;
pub mod ws_server;    // WebSocket 服务器及指令下发协议
pub mod radio_ctrl;    // 电台 UDP 协议控制模块

use std::sync::{Arc, Mutex, RwLock};
use axum::{Router, routing::get};
use tokio::sync::broadcast;

use crate::types::{AppState, DecodeBuffer, STATE, GLOBAL_TX, AUTO_MGR};
use crate::tasks::heartbeat_to_radio::{spawn_radio_heartbeat, spawn_status_heartbeat};
use crate::tasks::dsp::spawn_dsp_task;
use crate::tasks::tx_control::{spawn_tx_check_task, spawn_auto_qso_timer_task};
use crate::tasks::ft8_decode::spawn_decode_task;
use crate::tasks::radio_bridge_to_pc::spawn_radio_bridge_to_pc_task;
use crate::ft8_qso::notion_logger::NotionLogger;
use crate::ft8_qso::qso_auto_hunter::AutoQsoManager;
use crate::ft8_qso::psk_reporter::PskReporter;

#[tokio::main]
async fn main() {
    // --- 1. 初始化自动通联管理器与 Notion 日志服务 ---
    let (_notion_mgr, notion_tx) = NotionLogger::new();
    let mgr = Arc::new(Mutex::new(AutoQsoManager::new(notion_tx)));
    AUTO_MGR.set(mgr.clone()).ok();
    
    // 初始化本地呼号到哈希缓存，防止收到针对自己的哈希消息时解码为 <...>
    crate::ft8_codec::save_hash_call(config::MY_CALL);

    // --- 2. 初始化全局广播通道 (用于同步给所有 WS 客户端) ---
    let (tx, _) = broadcast::channel(1024);
    GLOBAL_TX.set(tx.clone()).ok();

    // --- 3. 初始化全局 AppState (保存电台参数与后端实时状态) ---
    let initial_ip = if crate::config::RADIO_ADDR.is_empty() { None } else { Some(crate::config::RADIO_ADDR.to_string()) };
    let state = Arc::new(RwLock::new(AppState {
        status: unsafe { std::mem::zeroed() },
        start_time: std::time::Instant::now(),
        current_if_hz: 12000,
        expect_regex_compiled: None,
        target_offset: 1000,
        radio_ip: initial_ip,
    }));
    {
        let mut s = state.write().unwrap();
        s.status.max_repeats = 4;
        s.status.tx_window_even = 1;
        s.status.pending_offset = 2950;
        s.status.auto_tx_mode = 0; 
        s.status.demod_mode = 0;
        s.status.ft8_decode_on = 1;
        s.status.audio_on = 0;
        s.status.usb_bw = 3000;
        s.status.lsb_bw = 3000;
        s.status.am_bw = 6000;
        // 瀑布图默认配置
        s.status.wf_speed = 3;
        s.status.wf_min_db = -120;
        s.status.wf_max_db = 0;
        s.status.wf_full_on = 1;
        s.status.wf_zoom_on = 1;
        s.status.rx_gain = 1.0;
    }
    STATE.set(state.clone()).ok();

    // --- 4. 生成 16 秒音频采样滑动缓冲区 (供 FT8 算法拉取) ---
    let decode_buf = Arc::new(Mutex::new(DecodeBuffer::new()));

    // --- 5. 启动无线电位置/热度上报服务 (PSK Reporter) ---
    let (psk_tx, psk_rx) = tokio::sync::mpsc::channel(100);
    tokio::spawn(async move {
        if let Ok(reporter) = PskReporter::init(psk_rx).await {
            reporter.run().await;
        }
    });

    // --- 6. 挂载所有后台长周期任务 ---
    
    // [任务A] 电台 UDP 心跳：维持与电台的连接活性 (需要动态感知 IP)
    spawn_radio_heartbeat(state.clone());
    
    // [任务B] 后端状态上报：向前端同步当前的 CPU 负载、解码统计等
    spawn_status_heartbeat(state.clone());
    
    // [任务C] DSP 处理集群：包括解调算法 (AM/LSB/USB) 和 瀑布图 FFT 生成
    spawn_dsp_task(state.clone(), decode_buf.clone());
    
    // [任务D] FT8 解码引擎：15秒窗口周期性触发一次全宽频率扫描解码
    spawn_decode_task(state.clone(), decode_buf.clone(), psk_tx);
    
    // [任务E] 电台日志桥接：实时捕获并转发电台的硬件日志与解析包
    spawn_radio_bridge_to_pc_task();
    
    // [任务F] 发射链路控制：根据时序决定是否下发发射指令，支持自动通联 (Mode 3)
    spawn_tx_check_task(state.clone());
    spawn_auto_qso_timer_task();

    // --- 7. 启动 WebSocket 控制前端接口 (基于 Axum) ---
    let app = Router::new()
        .route("/ws", get(crate::ws_server::ws_handler))
        .with_state(state.clone());

    println!("🚀 FT8 Backend Refactored V2 started on 0.0.0.0:8032");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8032").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ft8_codec::decode_ft8_block;
    use std::sync::mpsc;

    #[test]
    fn test_wav_decoding() {
        let wav_path = "/ssd/company_FT8/FT8/wav/test_01.wav";
        println!("Testing WAV decoding from: {}", wav_path);
        
        let result = crate::ft8_codec::wav_reader::WavHeader::read_from_file(wav_path);
        assert!(result.is_ok());
        
        let (_header, samples_i16) = result.unwrap();
        let f32_samples: Vec<f32> = samples_i16.iter().map(|&s| s as f32).collect();
        let (tx, rx) = mpsc::channel();
        
        decode_ft8_block(&f32_samples, tx);
        
        let mut count = 0;
        while let Ok(res) = rx.recv() {
            println!("PASS Result -> Freq: {:4} | SNR: {:+3} | Msg: {}", res.freq, res.snr, res.text);
            count += 1;
        }
        assert!(count >= 14);
    }
}
