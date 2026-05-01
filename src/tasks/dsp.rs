use tokio::net::UdpSocket;
use num_complex::Complex32;
use rustfft::FftPlanner;
use std::sync::{Arc, Mutex, RwLock};
use chrono::Timelike;

use crate::types::{
    AppState, DecodeBuffer, GLOBAL_TX, AUTO_MGR, 
    FFT_SIZE, TARGET_BINS, SAMPLE_RATE, DECIMATION_FACTOR, PKT_SIZE_IQ
};
use crate::utils::{decode_24le};
use crate::dsp::engine::SdrEngine;
use crate::dsp::adpcm::{AdpcmState, encode_adpcm};

pub fn spawn_dsp_task(state: Arc<RwLock<AppState>>, decode_buf: Arc<Mutex<DecodeBuffer>>) {
    tokio::spawn(async move {
        let socket = UdpSocket::bind("0.0.0.0:3333")
            .await
            .expect("无法绑定 3333 端口");
            
        let mut engine = SdrEngine::new(127);
        let mut phase = 0.0f32;
        let mut sample_count = 0;

        // 全频谱 (48kHz) FFT 资源
        let mut full_fft_input = Vec::with_capacity(FFT_SIZE);
        let mut full_spec_work = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
        let full_fft_planner = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        
        // 缩放频谱 (12kHz) FFT 资源
        let mut zoom_fft_input = Vec::with_capacity(FFT_SIZE);
        let mut zoom_spec_work = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
        let zoom_fft_planner = FftPlanner::new().plan_fft_forward(FFT_SIZE);

        // FT8 专属瀑布图 FFT 资源 (独立于 zoom, 使用 50% 重叠滑动窗口)
        let mut ft8_fft_input = Vec::with_capacity(FFT_SIZE);
        let mut ft8_spec_work = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
        let ft8_fft_planner = FftPlanner::new().plan_fft_forward(FFT_SIZE);

        let mut raw_buf = [0u8; 2048];
        let mut local_samples = Vec::with_capacity(128);
        let mut adpcm_state = AdpcmState::new();
        let mut audio_accum = Vec::with_capacity(256);
        let mut wf_counter = 0u8;
        let mut ft8_timeline_sec = 60u32; // FT8 15秒时间线标记，初始化为无效值

        println!("[任务2] 高性能瀑布图与音频 DSP 引擎已启动");

        loop {
            if let Ok((size, _)) = socket.recv_from(&mut raw_buf).await {
                if size != PKT_SIZE_IQ { continue; }

                let (cur_if, demod_mode, ft8_decode_on, audio_on, usb_bw, lsb_bw, am_bw, wf_speed, wf_full_on, wf_zoom_on, wf_min_db, wf_max_db, rx_gain) = { 
                    let s = state.read().unwrap();
                    (s.current_if_hz as f32, s.status.demod_mode, s.status.ft8_decode_on, 
                     s.status.audio_on, s.status.usb_bw, s.status.lsb_bw, s.status.am_bw,
                     s.status.wf_speed, s.status.wf_full_on, s.status.wf_zoom_on, s.status.wf_min_db, s.status.wf_max_db, s.status.rx_gain)
                };
                
                engine.update_taps(usb_bw, lsb_bw, am_bw);
                let phase_step = 2.0 * std::f32::consts::PI * cur_if / SAMPLE_RATE;

                for chunk in raw_buf[..size].chunks_exact(6) {
                    let r_i = decode_24le(&chunk[0..3]) as f32 / 8388608.0;
                    let r_q = decode_24le(&chunk[3..6]) as f32 / 8388608.0;
                    
                    // 1. 混频至中频
                    let lo = Complex32::from_polar(1.0, -phase);
                    let mixed = Complex32::new(r_i, -r_q) * lo;
                    phase = (phase + phase_step) % (2.0 * std::f32::consts::PI);

                    // ===== 瀑布图A: 全频谱 48kHz (0x12) - 独立开关 wf_full_on =====
                    if wf_full_on == 1 {
                        full_fft_input.push(mixed);
                        if full_fft_input.len() >= FFT_SIZE {
                            wf_counter = (wf_counter + 1) % wf_speed.max(1);
                            if wf_counter == 0 {
                                full_spec_work.copy_from_slice(&full_fft_input);
                                full_fft_planner.process(&mut full_spec_work);
                                
                                let mut pkt = Vec::with_capacity(1 + FFT_SIZE);
                                pkt.push(0x12); // 全频谱
                                for i in 0..FFT_SIZE {
                                    let db = 20.0 * (full_spec_work[i].norm() + 1e-9).log10();
                                    let val = (((db - wf_min_db as f32) / (wf_max_db as f32 - wf_min_db as f32)) * 255.0).clamp(0.0, 255.0) as u8;
                                    pkt.push(val);
                                }
                                if let Some(tx) = GLOBAL_TX.get() { let _ = tx.send(pkt); }
                            }
                            full_fft_input.clear();
                        }
                    } else { full_fft_input.clear(); }

                    // 2. 进入 SDR 引擎进行滤波与解调
                    let (f_i, f_q) = engine.process(mixed.re, mixed.im, demod_mode);
                    sample_count += 1;

                    // 每 4 个采样抽取一次 (12kHz)
                    if sample_count % DECIMATION_FACTOR == 0 {

                        // ===== 瀑布图B: 缩放频谱 12kHz (0x13) - 独立开关 wf_zoom_on =====
                        if wf_zoom_on == 1 {
                            zoom_fft_input.push(Complex32::new(f_i, f_q));
                            if zoom_fft_input.len() >= FFT_SIZE {
                                zoom_spec_work.copy_from_slice(&zoom_fft_input);
                                zoom_fft_planner.process(&mut zoom_spec_work);
                                
                                let mut pkt = Vec::with_capacity(1 + FFT_SIZE);
                                pkt.push(0x13); // 缩放频谱
                                for i in 0..FFT_SIZE {
                                    let db = 20.0 * (zoom_spec_work[i].norm() + 1e-9).log10();
                                    let val = (((db - wf_min_db as f32) / (wf_max_db as f32 - wf_min_db as f32)) * 255.0).clamp(0.0, 255.0) as u8;
                                    pkt.push(val);
                                }
                                if let Some(tx) = GLOBAL_TX.get() { let _ = tx.send(pkt); }
                                zoom_fft_input.clear();
                            }
                        } else { zoom_fft_input.clear(); }

                        // ===== 瀑布图C: FT8 专属瀑布图 (0x04) - 跟随 ft8_decode_on 开关 =====
                        // 完全独立于 48kHz 和 12kHz 瀑布图，使用独立 FFT 和 50% 重叠窗口
                        if ft8_decode_on == 1 {
                            ft8_fft_input.push(Complex32::new(f_i, f_q));

                            if ft8_fft_input.len() >= FFT_SIZE {
                                ft8_spec_work.copy_from_slice(&ft8_fft_input);
                                ft8_fft_planner.process(&mut ft8_spec_work);

                                let current_sec = chrono::Utc::now().second();

                                // FT8 瀑布图数据包 (0x04)
                                let mut pkt = Vec::with_capacity(1 + TARGET_BINS);
                                pkt.push(0x04);

                                // 15秒整点时间线标记 (255满载)
                                if current_sec % 15 == 0 && current_sec != ft8_timeline_sec {
                                    ft8_timeline_sec = current_sec;
                                    pkt.extend(std::iter::repeat(255).take(TARGET_BINS));
                                } else {
                                    for i in 0..TARGET_BINS {
                                        let db = 20.0 * (ft8_spec_work[i].norm() + 1e-9).log10();
                                        // FT8 瀑布图增益范围写死: -100dB ~ -10dB, 映射到 0~254
                                        let val = (((db + 100.0) / 90.0) * 254.0).clamp(0.0, 254.0) as u8;
                                        pkt.push(val);
                                    }
                                }
                                if let Some(tx) = GLOBAL_TX.get() { let _ = tx.send(pkt); }

                                // FT8 底噪分析：推送 FFT 幅度给 AutoQsoManager
                                let mut norms = Vec::with_capacity(TARGET_BINS);
                                for i in 0..TARGET_BINS { norms.push(ft8_spec_work[i].norm()); }
                                let is_txing = {
                                    let s = state.read().unwrap();
                                    let is_even_win = (current_sec % 30) < 15;
                                    s.status.auto_tx_mode > 0 && s.status.pending_msg[0] != 0
                                    && (s.status.tx_window_even == 1) == is_even_win
                                    && (current_sec % 15) < 13
                                };
                                if let Some(mgr) = AUTO_MGR.get() {
                                    if let Ok(mut mgr_lock) = mgr.lock() {
                                        mgr_lock.push_fft_noise(current_sec, &norms[..], is_txing);
                                    }
                                }

                                // 无重叠，完整步进：每行覆盖更长时间，瀑布图展示更多时间窗口
                                ft8_fft_input.clear();
                            }
                        } else { ft8_fft_input.clear(); }

                        // ===== 音频流输出 (ADPCM, 0x05) =====
                        if audio_on == 1 {
                            let s = ((f_i * rx_gain).clamp(-1.0, 1.0) * 32767.0) as i16;
                            audio_accum.push(s);
                            if audio_accum.len() >= 256 {
                                let mut pkt = vec![0x05]; // Audio pkt
                                pkt.extend_from_slice(&adpcm_state.valprev.to_le_bytes());
                                pkt.push(adpcm_state.index as u8);
                                for chunk in audio_accum.chunks_exact(2) {
                                    let c1 = encode_adpcm(chunk[0], &mut adpcm_state);
                                    let c2 = encode_adpcm(chunk[1], &mut adpcm_state);
                                    pkt.push(c1 | (c2 << 4));
                                }
                                if let Some(tx) = GLOBAL_TX.get() { let _ = tx.send(pkt); }
                                audio_accum.clear();
                            }
                        } else { audio_accum.clear(); }

                        // ===== FT8 采样缓冲推送 (跟随 ft8_decode_on) =====
                        if ft8_decode_on == 1 {
                             local_samples.push(((f_i * rx_gain).clamp(-1.0, 1.0) * 32767.0) as i16);
                             if local_samples.len() >= 128 {
                                 let mut locked_buf = decode_buf.lock().unwrap();
                                 for s in &local_samples { locked_buf.push(*s); }
                                 local_samples.clear();
                             }
                        } else { local_samples.clear(); }
                    }
                }
            }
        }
    });
}
