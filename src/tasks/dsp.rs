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
use crate::ft8_qso::qso_auto_hunter::AutoQsoManager;

pub fn spawn_dsp_task(state: Arc<RwLock<AppState>>, decode_buf: Arc<Mutex<DecodeBuffer>>) {
    tokio::spawn(async move {
        let socket = UdpSocket::bind("0.0.0.0:3333")
            .await
            .expect("无法绑定 3333 端口");
        let mut engine = SdrEngine::new(127);
        let mut phase = 0.0f32;
        let mut sample_count = 0;

        let mut fft_input = Vec::with_capacity(FFT_SIZE);
        let mut spec_work = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
        let fft_planner = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let mut raw_buf = [0u8; 2048];
        let mut last_marker_sec = -1i32;
        let mut local_samples = Vec::with_capacity(128);

        println!("[任务2] 高性能 DSP 引擎已启动 (FIR 127阶, 批量写缓冲开启)");

        let mut adpcm_state = AdpcmState::new();
        let mut audio_accum = Vec::with_capacity(256);

        loop {
            if let Ok((size, _)) = socket.recv_from(&mut raw_buf).await {
                if size != PKT_SIZE_IQ { continue; }

                let (cur_if, demod_mode, ft8_decode_on, audio_on, usb_bw, lsb_bw, am_bw) = { 
                    let s = state.read().unwrap();
                    (s.current_if_hz as f32, s.status.demod_mode, s.status.ft8_decode_on, 
                     s.status.audio_on, s.status.usb_bw, s.status.lsb_bw, s.status.am_bw)
                };
                engine.update_taps(usb_bw, lsb_bw, am_bw);
                let phase_step = 2.0 * std::f32::consts::PI * cur_if / SAMPLE_RATE;

                for chunk in raw_buf[..size].chunks_exact(6) {
                    let r_i = decode_24le(&chunk[0..3]) as f32 / 8388608.0;
                    let r_q = decode_24le(&chunk[3..6]) as f32 / 8388608.0;
                    let lo = Complex32::from_polar(1.0, -phase);
                    let mixed = Complex32::new(r_i, -r_q) * lo;
                    phase = (phase + phase_step) % (2.0 * std::f32::consts::PI);

                    let (f_i, f_q) = engine.process(mixed.re, mixed.im, demod_mode);
                    sample_count += 1;
                    if sample_count % DECIMATION_FACTOR == 0 {
                        // 音频流处理
                        if audio_on == 1 {
                            let s = (f_i.clamp(-1.0, 1.0) * 32767.0) as i16;
                            audio_accum.push(s);
                            if audio_accum.len() >= 256 {
                                let mut pkt = vec![0x05];
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

                        // A. DecodeBuffer
                        if demod_mode == 0 && ft8_decode_on == 1 {
                            local_samples.push((f_i.clamp(-1.0, 1.0) * 32767.0) as i16);
                            if local_samples.len() >= 128 {
                                let mut locked_buf = decode_buf.lock().unwrap();
                                for s in &local_samples { locked_buf.push(*s); }
                                local_samples.clear();
                            }
                        } else { local_samples.clear(); }

                        // B. Waterfall
                        if ft8_decode_on == 1 {
                            fft_input.push(Complex32::new(f_i, f_q));
                            if fft_input.len() >= FFT_SIZE {
                                spec_work.copy_from_slice(&fft_input);
                                fft_planner.process(&mut spec_work);
                                let mut waterfall = Vec::with_capacity(1 + TARGET_BINS);
                                waterfall.push(0x04);
                                let current_sec = chrono::Utc::now().second();
                                
                                let mut norms = Vec::with_capacity(TARGET_BINS);
                                for i in 0..TARGET_BINS { norms.push(spec_work[i].norm()); }
                                
                                let is_txing = {
                                    let s = state.read().unwrap();
                                    let is_even_win = (current_sec % 30) < 15;
                                    s.status.auto_tx_mode > 0 && s.status.pending_msg[0] != 0
                                    && (s.status.tx_window_even == 1) == is_even_win
                                    && (current_sec % 15) < 13
                                };
                                if let Some(mgr) = AUTO_MGR.get() {
                                    if let Ok(mut mgr_lock) = mgr.lock() {
                                        let mgr_lock: &mut AutoQsoManager = &mut *mgr_lock;
                                        mgr_lock.push_fft_noise(current_sec, &norms[..], is_txing);
                                    }
                                }

                                if current_sec % 15 == 0 && current_sec as i32 != last_marker_sec {
                                    last_marker_sec = current_sec as i32;
                                    waterfall.extend(std::iter::repeat(255).take(TARGET_BINS));
                                } else {
                                    for i in 0..TARGET_BINS {
                                        let val = (((20.0 * (spec_work[i].norm() + 1e-9).log10() + 100.0) / 90.0) * 254.0).clamp(0.0, 254.0) as u8;
                                        waterfall.push(val);
                                    }
                                }
                                if let Some(tx) = GLOBAL_TX.get() { let _ = tx.send(waterfall); }
                                fft_input.drain(0..FFT_SIZE / 2);
                            }
                        } else { fft_input.clear(); }
                    }
                }
            }
        }
    });
}
