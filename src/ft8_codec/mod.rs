pub mod wav_reader;
mod ldpc_decoder;
mod packjt77;
mod ldpc_constants;
mod ldpc_gen;
pub mod subtraction;

pub use packjt77::save_hash_call;
pub use packjt77::resolve_hashes;

use std::f32::consts::PI;
use num_complex::Complex32;
use std::collections::HashSet;
use std::time::Instant;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use rustfft::{Fft, FftPlanner};
use rayon::prelude::*;

unsafe extern "C" {
    pub fn encode_ft8_symbols(msg: *const libc::c_char, tones: *mut i32);
}

lazy_static::lazy_static! {
    // 预计算 Costas 同步相位
    static ref CSYNC: [[Complex32; 32]; 7] = precompute_csync();
    
    // 预计算 Nuttall 窗
    static ref NUTTALL_WINDOW: Vec<f32> = precompute_nuttall_window();

    // 全局共享的 FFT 计划柜 (线程安全)
    static ref FFT_PLAN_3840: Arc<dyn Fft<f32>> = FftPlanner::new().plan_fft_forward(3840);
    static ref FFT_PLAN_192000: Arc<dyn Fft<f32>> = FftPlanner::new().plan_fft_forward(192000);
    static ref IFFT_PLAN_3200: Arc<dyn Fft<f32>> = FftPlanner::new().plan_fft_inverse(3200);
    static ref FFT_PLAN_32: Arc<dyn Fft<f32>> = FftPlanner::new().plan_fft_forward(32);
}

// --- 常量定义 ---
const NSPS: usize = 1920;
const NFFT1: usize = 2 * NSPS;
const NH1: usize = NFFT1 / 2;
const NSTEP: usize = NSPS / 4;
const NHSYM: usize = (15 * 12000) / NSTEP - 3;

/// FT8 同步检测结果
#[derive(Clone)]
pub struct SyncCandidate {
    pub freq: f32,
    pub dt: f32,
    pub sync: f32,
    pub noise: f32,
}

#[derive(Debug, Clone)]
pub struct Ft8DecodeResult {
    pub freq: f32,
    pub dt: f32,
    pub snr: i32,
    pub text: String,
    pub decode_time_ms: u32,
    pub sender_call: Option<String>,
    pub receiver_call: Option<String>,
    pub grid: Option<String>,
    pub region: Option<String>,
}

/// 供后端调用的流式解码入口 (1:1 移植 0316 核心逻辑)
pub fn decode_ft8_block(dd: &[f32], tx_result: Sender<Ft8DecodeResult>) {
    let start_time = Instant::now(); // 记录采样传入时间
    let mut current_dd = dd.to_vec();
    let seen_messages = Arc::new(std::sync::Mutex::new(HashSet::new()));
    
    let ifft_plan = Arc::clone(&IFFT_PLAN_3200);
    let fft32 = Arc::clone(&FFT_PLAN_32);
    let mut sub_planner = FftPlanner::new();

    for _pass in 1..=3 {
        let syncmin = 1.3f32; 
        let maxcand = 600;

        // 1. 同步搜索 + 噪声基线
        let (candidates, sbase_vec) = run_sync8(&current_dd, 100, 3500, syncmin, maxcand, &NUTTALL_WINDOW);
        if candidates.is_empty() { continue; }

        // 2. 全频谱计算
        let whole_spectrum = compute_whole_spectrum(&current_dd);
        let dedup_candidates = dedup_candidates(&candidates, 3.125);

        let tx_clone = tx_result.clone();
        let seen_clone = Arc::clone(&seen_messages);

        // 3. 并行解码
        let pass_new_decodes: Vec<_> = dedup_candidates.par_iter()
            .filter_map(|cand| {
                let f_idx = (cand.freq / 3.125).round() as usize;
                let xb = if f_idx < sbase_vec.len() { sbase_vec[f_idx] } else { 1.0 };
                
                if let Some((msg_obj, snr, itone, f_ref, i_ref)) = run_ft8_decode_candidate_fast(
                    &whole_spectrum, cand.freq, cand.dt, &ifft_plan, &CSYNC, &fft32, xb
                ) {
                    if is_likely_valid_msg(&msg_obj.text) {
                        let mut seen = seen_clone.lock().unwrap();
                        if seen.insert(msg_obj.text.clone()) {
                             // --- 识别发送者与接收者逻辑 ---
                          let (sender, receiver) = match (&msg_obj.call_1, &msg_obj.call_2) {
                // 情况 1: "CQ呼号" 或 "QRZ呼号" -> Call2 是发送者，Call1 是 CQ/QRZ
                (Some(c1), Some(c2)) if c1.starts_with("CQ") || c1 == "QRZ" => {
                    (Some(c2.clone()), Some(c1.clone()))
                }
                // 情况 2: 标准通联 "接收者 发送者" -> Call2 是发送者，Call1 是 接收者
                (Some(c1), Some(c2)) => {
                    (Some(c2.clone()), Some(c1.clone()))
                }
                // 情况 3: 只有一个呼号的情况
                (None, Some(c2)) => (Some(c2.clone()), None),
                (Some(c1), None) => (Some(c1.clone()), None),
                _ => (None, None),
            };

                            let dt_refined = (i_ref as f32 - 1.0) / 200.0 - 0.5;
                            let elapsed_ms = start_time.elapsed().as_millis() as u32;
                            let _ = tx_clone.send(Ft8DecodeResult {
                                freq: f_ref,
                                dt: dt_refined,
                                snr: snr as i32,
                                text: msg_obj.text.clone(),
                                decode_time_ms: elapsed_ms,
                            sender_call: sender,    // 注入呼号
                            receiver_call: receiver,
                            grid: msg_obj.grid.clone(),     // 注入网格
                            region: None,
                            });
                            return Some((itone, f_ref, dt_refined));
                        }
                    }
                }
                None
            }).collect();

        // 4. 信号剥离
        for (itone, freq, dt) in &pass_new_decodes {
            subtraction::subtract_ft8(&mut current_dd, itone, *freq, *dt, 12000.0, 1920, &mut sub_planner);
        }
    }
}


pub fn p_main() {
    println!("========================================");
    println!("FT8 Rust 解码器库已加载");
    println!("========================================");
}

// --- 核心算法函数 ---

fn ft8_downsample_fast(cx: &[Complex32], f0: f32, ifft2: &Arc<dyn rustfft::Fft<f32>>) -> Vec<Complex32> {
    const NFFT1_LARGE: usize = 192000;
    const NFFT2: usize = 3200;
    let df = 12000.0 / NFFT1_LARGE as f32;
    let baud = 12000.0 / 1920.0;

    let ft = f0 + 8.5 * baud;
    let it = (ft / df).round() as i32;
    let fb = f0 - 1.5 * baud;
    let ib = (fb / df).round() as i32;

    let mut c1 = vec![Complex32::new(0.0, 0.0); NFFT2];
    let mut k: usize = 0;
    for i in ib..=it {
        if k < NFFT2 && i >= 0 && (i as usize) < cx.len() {
            c1[k] = cx[i as usize];
            k += 1;
        }
    }

    let ntaper = 101.min(k / 2);
    for i in 0..ntaper {
        let frac = 0.5 * (1.0 - (i as f32 * PI / 100.0).cos());
        c1[i] *= frac;
        if k > i { c1[k - 1 - i] *= frac; }
    }

    let i0_round = (f0 / df).round() as i32;
    let rotation = ((i0_round - ib) % NFFT2 as i32 + NFFT2 as i32) % NFFT2 as i32;
    if rotation > 0 { c1.rotate_left(rotation as usize); }

    ifft2.process(&mut c1);

    let final_fac = 1.0f32 / 24000.0;
    for val in c1.iter_mut() { *val *= final_fac; }

    c1
}

fn run_ft8_decode_candidate_fast(
    cx: &[Complex32],
    f1_start: f32,
    xdt_start: f32,
    ifft_plan: &Arc<dyn rustfft::Fft<f32>>,
    csync: &[[Complex32; 32]; 7],
    fft32: &Arc<dyn rustfft::Fft<f32>>,
    xbase: f32,
) -> Option<(packjt77::Ft8Message, f32, Vec<u8>, f32, i32)> {
    let mut f1 = f1_start;
    let fs2 = 200.0f32;
    let dt2 = 1.0 / fs2;

    // 1. 第一次下采样
    let cd0 = ft8_downsample_fast(cx, f1, ifft_plan);

    // 2. 初始时间搜索
    let i0: i32 = ((xdt_start + 0.5) * fs2).round() as i32;
    let mut smax = -1.0f32;
    let mut ibest: i32 = i0;

    for idt in (i0 - 10)..=(i0 + 10) {
        let sync = sync8d_fast_simple(&cd0, idt, csync);
        if sync > smax {
            smax = sync;
            ibest = idt;
        }
    }

    // 3. 频率细扫
    smax = 0.0;
    let mut delfbest = 0.0f32;
    for ifr in -5..=5 {
        let delf = ifr as f32 * 0.5;
        let sync = sync8d_with_freq_twist(&cd0, ibest, csync, delf, dt2);
        if sync > smax {
            smax = sync;
            delfbest = delf;
        }
    }
    f1 += delfbest;
    // 优化：如果频率微调为 0，跳过第二次 IFFT
    let cd0 = if delfbest != 0.0 {
        ft8_downsample_fast(cx, f1, ifft_plan)
    } else {
        cd0
    };

    // 4. 细时间搜索
    let mut ss = [0.0f32; 9];
    for idt in -4..=4 {
        ss[(idt + 4) as usize] = sync8d_fast_simple(&cd0, ibest + idt, csync);
    }

    let mut local_smax = -1.0;
    let mut iloc = 0;
    for i in 0..9 {
        if ss[i] > local_smax {
            local_smax = ss[i];
            iloc = i;
        }
    }
    ibest = (iloc as i32 - 4) + ibest;

    // 5. 符号提取 (NN=79) — 使用共享的 fft32
    let mut cs = vec![[Complex32::new(0.0, 0.0); 8]; 80];
    let mut s8 = vec![[0.0f32; 8]; 80];
    let mut csymb = [Complex32::new(0.0, 0.0); 32]; // 复用缓冲区

    for k in 1..=79 {
        let i1 = ibest + (k as i32 - 1) * 32;

        // 清零缓冲区
        for v in csymb.iter_mut() { *v = Complex32::new(0.0, 0.0); }

        if i1 >= 0 && (i1 + 31) < cd0.len() as i32 {
            let start = i1 as usize;
            csymb[..32].copy_from_slice(&cd0[start..start + 32]);
        }

        fft32.process(&mut csymb);

        for m in 0..8 {
            cs[k][m] = csymb[m];
            s8[k][m] = csymb[m].norm();
        }
    }

    // 6. Hard Sync Check
    let costas = [3, 1, 4, 0, 6, 5, 2];
    let mut nsync = 0;
    for i in 0..7 {
        if get_max_index(&s8[i + 1]) == costas[i] { nsync += 1; }
        if get_max_index(&s8[i + 37]) == costas[i] { nsync += 1; }
        if get_max_index(&s8[i + 73]) == costas[i] { nsync += 1; }
    }
    if nsync <= 4 { return None; }

    // 7. 解码循环
    for ipass in 1..=4 {
        let llrz = match ipass {
            1 => compute_llrs_nsym(&cs, 1),
            2 => compute_llrs_nsym(&cs, 2),
            3 => compute_llrs_nsym(&cs, 3),
            4 => compute_llrs_bmetd_from_cs(&cs),
            _ => unreachable!(),
        };

        if let Some(res) = ldpc_decode_with_llr_fast(llrz, ipass, nsync) {
            if let Some(msg_obj) = parse_ft8_message(&res.message_bits) {
                let itone = get_tones_from_result(&res);
                let snr = calculate_snr_fortran(&s8, &itone, xbase);

                if is_likely_valid_msg(&msg_obj.text) {
                    return Some((msg_obj, snr, itone.to_vec(), f1, ibest));
                }
            }
        }
    }
    None
}

fn compute_llrs_bmetd_from_cs(cs: &[[Complex32; 8]]) -> Vec<f32> {
    let mut llr = vec![0.0f32; 174];
    let gray_map: [usize; 8] = [0, 1, 3, 2, 5, 6, 4, 7];

    for ihalf in 0..2 {
        for k in 0..29 {
            let ks = if ihalf == 0 { k + 7 + 1 } else { k + 43 + 1 };
            let mut s2 = [0.0f32; 8];
            for i3 in 0..8 {
                s2[i3] = cs[ks][gray_map[i3]].norm();
            }

            for ib in 0..3 {
                let mut max0 = -1e30f32;
                let mut max1 = -1e30f32;
                for val in 0..8 {
                    if (val >> (2 - ib)) & 1 == 1 {
                        max1 = max1.max(s2[val]);
                    } else {
                        max0 = max0.max(s2[val]);
                    }
                }
                let diff = max1 - max0;
                let den = max1.max(max0);
                let bit_idx = (k * 3) + (ihalf * 87) + ib;
                if bit_idx < 174 {
                    llr[bit_idx] = if den > 0.0 { diff / den } else { 0.0 };
                }
            }
        }
    }
    llr
}

fn compute_llrs_nsym(cs: &[[Complex32; 8]], nsym: usize) -> Vec<f32> {
    let mut llr = vec![0.0f32; 174];
    let gray_map: [usize; 8] = [0, 1, 3, 2, 5, 6, 4, 7];

    for ihalf in 0..2 {
        for k in (0..29).step_by(nsym) {
            let ks = if ihalf == 0 { k + 7 + 1 } else { k + 43 + 1 };
            let nt = 1 << (3 * nsym);
            let mut s2 = vec![0.0f32; nt];

            for i in 0..nt {
                let i1 = i / 64;
                let i2 = (i & 63) / 8;
                let i3 = i & 7;

                if nsym == 1 {
                    s2[i] = cs[ks][gray_map[i3]].norm();
                } else if nsym == 2 {
                    let combined = cs[ks][gray_map[i2]] + cs[ks + 1][gray_map[i3]];
                    s2[i] = combined.norm();
                } else if nsym == 3 {
                    let combined = cs[ks][gray_map[i1]] + cs[ks + 1][gray_map[i2]] + cs[ks + 2][gray_map[i3]];
                    s2[i] = combined.norm();
                }
            }

            let ibmax = (3 * nsym) - 1;
            for ib in 0..=ibmax {
                let mut max0 = -1e30f32;
                let mut max1 = -1e30f32;
                for val in 0..nt {
                    if (val >> (ibmax - ib)) & 1 == 1 {
                        max1 = max1.max(s2[val]);
                    } else {
                        max0 = max0.max(s2[val]);
                    }
                }
                let bit_idx = (k * 3) + (ihalf * 87) + ib;
                if bit_idx < 174 {
                    llr[bit_idx] = max1 - max0;
                }
            }
        }
    }
    llr
}

#[inline(always)]
fn sync8d_fast_simple(cd0: &[Complex32], i0: i32, csync: &[[Complex32; 32]; 7]) -> f32 {
    let mut sync_accum = 0.0f32;
    let cd0_len = cd0.len() as i32;
    for i in 0..7 {
        for &offset in &[0i32, 36, 72] {
            let i_start = i0 + (i as i32 + offset) * 32;
            if i_start >= 0 && (i_start + 31) < cd0_len {
                let mut z = Complex32::new(0.0, 0.0);
                let start = i_start as usize;
                for j in 0..32 {
                    z += cd0[start + j] * csync[i][j].conj();
                }
                sync_accum += z.norm_sqr();
            }
        }
    }
    sync_accum
}

/// 带频率微调的 sync8d
fn sync8d_with_freq_twist(cd0: &[Complex32], i0: i32, csync: &[[Complex32; 32]; 7], delf: f32, dt2: f32) -> f32 {
    let twopi = 2.0 * PI;
    let dphi = twopi * delf * dt2;

    let mut ctwk = [Complex32::new(0.0, 0.0); 32];
    let mut phi = 0.0f32;
    for i in 0..32 {
        ctwk[i] = Complex32::new(phi.cos(), phi.sin());
        phi = (phi + dphi) % twopi;
    }

    let mut sync_accum = 0.0f32;
    let cd0_len = cd0.len() as i32;
    for i in 0..7 {
        for &offset in &[0i32, 36, 72] {
            let i_start = i0 + (i as i32 + offset) * 32;
            if i_start >= 0 && (i_start + 31) < cd0_len {
                let mut z = Complex32::new(0.0, 0.0);
                let start = i_start as usize;
                for j in 0..32 {
                    z += cd0[start + j] * ctwk[j].conj() * csync[i][j].conj();
                }
                sync_accum += z.norm_sqr();
            }
        }
    }
    sync_accum
}

fn calculate_snr_fortran(s8: &[[f32; 8]], itone: &[u8; 79], xb: f32) -> f32 {
    let mut xsig = 0.0f32;
    for i in 1..=79 {
        let t = itone[i - 1] as usize;
        xsig += s8[i][t].powi(2);
    }

    let xbase_scaled = xb * 1.0e-4;
    let arg = (xsig / xbase_scaled / 3.0e6 - 1.0).max(0.01);

    let snr = 10.0 * arg.log10() - 27.0;
    snr.max(-24.0)
}

#[inline(always)]
fn get_max_index(row: &[f32; 8]) -> usize {
    let mut max_v = row[0];
    let mut idx = 0;
    for i in 1..8 {
        if row[i] > max_v {
            max_v = row[i];
            idx = i;
        }
    }
    idx
}

// --- 预计算与工具函数 ---

fn precompute_csync() -> [[Complex32; 32]; 7] {
    let costas_7x7 = [3, 1, 4, 0, 6, 5, 2];
    let mut csync = [[Complex32::new(0.0, 0.0); 32]; 7];
    for i in 0..7 {
        let dphi = 2.0 * PI * costas_7x7[i] as f32 / 32.0;
        for j in 0..32 {
            let (s, c) = (dphi * j as f32).sin_cos();
            csync[i][j] = Complex32::new(c, s);
        }
    }
    csync
}


/// 预计算 Nuttall 窗，避免每个 pass 重复生成
fn precompute_nuttall_window() -> Vec<f32> {
    let nfft1 = 3840;
    let mut window = vec![0.0f32; nfft1];
    let mut win_sum = 0.0f32;
    for i in 0..nfft1 {
        let x = i as f32 / nfft1 as f32;
        let w = 0.355768 - 0.487396 * (2.0 * PI * x).cos()
            + 0.144232 * (4.0 * PI * x).cos()
            - 0.012604 * (6.0 * PI * x).cos();
        window[i] = w;
        win_sum += w;
    }
    let nsps = 1920.0;
    let fac = (nsps * 2.0 / 300.0) / win_sum;
    for w in window.iter_mut() { *w *= fac; }
    window
}

fn compute_whole_spectrum(dd: &[f32]) -> Vec<Complex32> {
    const NFFT_LARGE: usize = 192000;
    let mut x_large = vec![Complex32::new(0.0, 0.0); NFFT_LARGE];
    let len = NFFT_LARGE.min(dd.len());
    for i in 0..len {
        x_large[i] = Complex32::new(dd[i], 0.0);
    }
    FFT_PLAN_192000.process(&mut x_large);
    x_large
}

fn run_sync8(dd: &[f32], nfa: i32, nfb: i32, syncmin: f32, maxcand: usize, window: &[f32]) -> (Vec<SyncCandidate>, Vec<f32>) {
    let mut s = vec![0.0f32; NH1 * NHSYM];
    let mut sbase = vec![0.0f32; NH1];
    let tstep = NSTEP as f32 / 12000.0;
    let df = 12000.0 / NFFT1 as f32;
    let mut x = vec![Complex32::new(0.0, 0.0); NFFT1];

    for j in 0..NHSYM {
        let ia = j * NSTEP;
        if ia + NSPS > dd.len() { break; }
        for i in 0..NFFT1 {
            x[i] = if i < NSPS { Complex32::new(dd[ia + i], 0.0) } else { Complex32::new(0.0, 0.0) };
        }
        FFT_PLAN_3840.process(&mut x);
        for i in 0..NH1 {
            s[i * NHSYM + j] = x[i].norm_sqr();
        }
    }

    let mut planner = FftPlanner::new();
    get_spectrum_baseline(dd, nfa, nfb, &mut sbase, window, &mut planner);

    // 预计算 s_sum8: 每个 bin (i, j) 对应的 8 个 Tones 之和
    let nfos = NFFT1 / NSPS;
    let mut s_sum8 = vec![0.0f32; NH1 * NHSYM];
    s_sum8.par_chunks_exact_mut(NHSYM).enumerate().for_each(|(i, chunk)| {
        if i < NH1 - nfos * 8 {
            for j in 0..NHSYM {
                let mut sum = 0.0;
                for k in 0..8 { sum += s[(i + nfos * k) * NHSYM + j]; }
                chunk[j] = sum;
            }
        }
    });

    let ia_freq = ((nfa as f32 / df).round() as usize).max(1);
    let ib_freq = (nfb as f32 / df).round() as usize;
    let jstrt = (0.5 / tstep) as i32;
    let costas = [3usize, 1, 4, 0, 6, 5, 2];
    let mut red = vec![0.0f32; NH1];
    let mut red2 = vec![0.0f32; NH1];
    let mut jpeak = vec![0i32; NH1];
    let mut jpeak2 = vec![0i32; NH1];

    // 并行计算同步搜索 (最大热点)
    let sync_results: Vec<_> = (ia_freq..=ib_freq).into_par_iter().map(|i| {
        let mut max_val_10 = -1.0f32;
        let mut max_idx_10 = 0i32;
        let mut max_val_62 = -1.0f32;
        let mut max_idx_62 = 0i32;

        let mut f_costas = [0usize; 7];
        for n in 0..7 { f_costas[n] = i + nfos * costas[n]; }

        for j in -62i32..=62 {
            let mut ta = 0.0f32;
            let mut tb = 0.0f32;
            let mut tc = 0.0f32;
            let mut t0a = 0.0f32;
            let mut t0b = 0.0f32;
            let mut t0c = 0.0f32;

            for n in 0..7 {
                let m = j + jstrt + 4 * n as i32;
                if m >= 1 && m <= NHSYM as i32 {
                    let idx = (m - 1) as usize;
                    if f_costas[n] < NH1 { ta += s[f_costas[n] * NHSYM + idx]; }
                    t0a += s_sum8[i * NHSYM + idx];
                }

                let m2 = j + jstrt + 4 * (36 + n as i32);
                if m2 >= 1 && m2 <= NHSYM as i32 {
                    let idx = (m2 - 1) as usize;
                    if f_costas[n] < NH1 { tb += s[f_costas[n] * NHSYM + idx]; }
                    t0b += s_sum8[i * NHSYM + idx];
                }

                let m3 = j + jstrt + 4 * (72 + n as i32);
                if m3 >= 1 && m3 <= NHSYM as i32 {
                    let idx = (m3 - 1) as usize;
                    if f_costas[n] < NH1 { tc += s[f_costas[n] * NHSYM + idx]; }
                    t0c += s_sum8[i * NHSYM + idx];
                }
            }

            let t_abc = ta + tb + tc;
            let t0_abc = (t0a + t0b + t0c - t_abc) / 7.0;
            let sync_abc = if t0_abc > 0.0 { t_abc / t0_abc } else { 0.0 };

            let t_bc = tb + tc;
            let t0_bc = (t0b + t0c - t_bc) / 7.0;
            let sync_bc = if t0_bc > 0.0 { t_bc / t0_bc } else { 0.0 };

            let sync2d = sync_abc.max(sync_bc);

            if j >= -10 && j <= 10 {
                if sync2d > max_val_10 { max_val_10 = sync2d; max_idx_10 = j; }
            }
            if sync2d > max_val_62 { max_val_62 = sync2d; max_idx_62 = j; }
        }

        (i, max_val_10, max_idx_10, max_val_62, max_idx_62)
    }).collect();

    for (i, mv10, mi10, mv62, mi62) in sync_results {
        red[i] = mv10;
        jpeak[i] = mi10;
        red2[i] = mv62;
        jpeak2[i] = mi62;
    }

    // 40th percentile baseline
    let iz = ib_freq - ia_freq + 1;
    let mut sorted_red: Vec<f32> = red[ia_freq..=ib_freq].to_vec();
    sorted_red.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let npctile = ((0.40 * iz as f32) as usize).max(1);
    let base = sorted_red[npctile - 1].max(1e-10);

    let mut sorted_red2: Vec<f32> = red2[ia_freq..=ib_freq].to_vec();
    sorted_red2.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let base2 = sorted_red2[npctile - 1].max(1e-10);

    let mut candidates = Vec::with_capacity(maxcand);
    for i in ia_freq..=ib_freq {
        let r1 = red[i] / base;
        if r1 >= syncmin {
            let dt = (jpeak[i] as f32 - 0.5) * tstep;
            let freq = i as f32 * df;
            candidates.push(SyncCandidate { freq, dt, sync: r1, noise: sbase[i] });
        }

        if jpeak[i] != jpeak2[i] {
            let r2 = red2[i] / base2;
            if r2 >= syncmin {
                let dt = (jpeak2[i] as f32 - 0.5) * tstep;
                let freq = i as f32 * df;
                candidates.push(SyncCandidate { freq, dt, sync: r2, noise: sbase[i] });
            }
        }
    }
    candidates.sort_unstable_by(|a, b| b.sync.partial_cmp(&a.sync).unwrap_or(std::cmp::Ordering::Equal));
    candidates.truncate(maxcand);
    (candidates, sbase)
}

fn get_spectrum_baseline(dd: &[f32], nfa: i32, nfb: i32, sbase: &mut [f32], window: &[f32], planner: &mut FftPlanner<f32>) {
    let nfft1 = 3840;
    let nst = nfft1 / 2;
    let nh1 = nfft1 / 2 + 1;
    let df = 12000.0 / nfft1 as f32;

    let fft_plan = planner.plan_fft_forward(nfft1);
    let mut savg = vec![0.0f32; nh1];
    let mut x = vec![Complex32::new(0.0, 0.0); nfft1];

    // 分段计算功率谱（复用 x 缓冲区）
    let nf = dd.len() / nst - 1;
    for j in 0..nf {
        let ia = j * nst;
        let ib = ia + nfft1;
        if ib > dd.len() { break; }

        for i in 0..nfft1 {
            x[i] = Complex32::new(dd[ia + i] * window[i], 0.0);
        }
        fft_plan.process(&mut x);

        for i in 0..nh1 { savg[i] += x[i].norm_sqr(); }
    }

    // baseline.f90 的 1:1 移植
    let ia_idx = ((nfa as f32 / df).round() as usize).max(0);
    let ib_idx = ((nfb as f32 / df).round() as usize).min(nh1 - 1);

    let mut s_db = vec![0.0f32; nh1];
    for i in ia_idx..=ib_idx {
        s_db[i] = 10.0 * (savg[i].max(1e-10).log10());
    }

    let nseg = 10;
    let npct = 10;
    let nlen = (ib_idx - ia_idx + 1) / nseg;
    let i0 = (ib_idx - ia_idx + 1) / 2;

    let mut x_vals = Vec::with_capacity(1000);
    let mut sbase_vals = Vec::with_capacity(1000);

    for n in 1..=nseg {
        let ja = ia_idx + (n - 1) * nlen;
        let jb = ja + nlen - 1;
        if jb > ib_idx { continue; }

        let mut segment_db = s_db[ja..=jb].to_vec();
        segment_db.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let idx_pct = (segment_db.len() * npct / 100).min(segment_db.len() - 1);
        let base = segment_db[idx_pct];

        for i in ja..=jb {
            if s_db[i] <= base && x_vals.len() < 1000 {
                x_vals.push((i as f64 - i0 as f64) as f64);
                sbase_vals.push(s_db[i] as f64);
            }
        }
    }

    // polyfit 4阶多项式拟合 (WSJT-X 1:1)
    let mut a = [0.0f64; 5];
    polyfit(&x_vals, &sbase_vals, x_vals.len(), 5, &mut a);

    for i in ia_idx..=ib_idx {
        let t = i as f64 - i0 as f64;
        let fit_db = a[0] + t * (a[1] + t * (a[2] + t * (a[3] + t * a[4])));
        sbase[i] = 10.0f32.powf((fit_db as f32 + 0.65) / 10.0);
    }
}

// WSJT-X: lib/determ.f90 的 1:1 移植
fn determ(array_in: &[[f64; 10]; 10], norder: usize) -> f64 {
    let mut array = *array_in;
    let mut det = 1.0f64;

    for k in 0..norder {
        if array[k][k] == 0.0 {
            let mut found = false;
            let mut j_found = 0;
            for j in k..norder {
                if array[k][j] != 0.0 {
                    found = true;
                    j_found = j;
                    break;
                }
            }
            if !found { return 0.0; }
            for i in k..norder {
                let s8_val = array[i][j_found];
                array[i][j_found] = array[i][k];
                array[i][k] = s8_val;
            }
            det = -det;
        }

        det *= array[k][k];
        if k < norder - 1 {
            let k1 = k + 1;
            for i in k1..norder {
                for j in k1..norder {
                    array[i][j] -= array[i][k] * array[k][j] / array[k][k];
                }
            }
        }
    }
    det
}

// WSJT-X: lib/polyfit.f90 的 1:1 移植 (mode=0 专用)
fn polyfit(x: &[f64], y: &[f64], npts: usize, nterms: usize, a: &mut [f64]) {
    let mut sumx = [0.0f64; 10];
    let mut sumy = [0.0f64; 10];
    let mut array = [[0.0f64; 10]; 10];

    let nmax = 2 * nterms - 1;

    for i in 0..npts {
        let xi = x[i];
        let yi = y[i];

        let mut xterm = 1.0f64;
        for n in 0..nmax {
            sumx[n] += xterm;
            xterm *= xi;
        }

        let mut yterm = yi;
        for n in 0..nterms {
            sumy[n] += yterm;
            yterm *= xi;
        }
    }

    for j in 0..nterms {
        for k in 0..nterms {
            array[j][k] = sumx[j + k];
        }
    }

    let delta = determ(&array, nterms);

    if delta == 0.0 {
        for val in a.iter_mut() { *val = 0.0; }
    } else {
        for l in 0..nterms {
            let mut array_l = [[0.0f64; 10]; 10];
            for j in 0..nterms {
                for k in 0..nterms {
                    array_l[j][k] = sumx[j + k];
                }
                array_l[j][l] = sumy[j];
            }
            a[l] = determ(&array_l, nterms) / delta;
        }
    }
}

fn ldpc_decode_with_llr_fast(mut llr: Vec<f32>, pass: usize, nsync: usize) -> Option<ldpc_decoder::DecodeResult> {
    normalize_llr(&mut llr);
    let apmask = vec![0u8; 174];
    let maxosd = if pass == 1 { 0 } else { if nsync > 12 { 1 } else { 0 } };
    let res = ldpc_decoder::decode_174_91(&llr, &apmask, maxosd, maxosd as i32);
    if res.success { Some(res) } else { None }
}

#[inline]
fn normalize_llr(llr: &mut [f32]) {
    let n = llr.len() as f32;
    let sum: f32 = llr.iter().sum();
    let sum_sq: f32 = llr.iter().map(|x| x * x).sum();
    let bmetav = sum / n;
    let var = (sum_sq / n) - bmetav * bmetav;
    let std_dev = var.max(1e-10).sqrt();
    let scale = 2.83 / std_dev;
    for x in llr.iter_mut() { *x *= scale; }
}

fn get_tones_from_result(res: &ldpc_decoder::DecodeResult) -> [u8; 79] {
    let mut tones = [0u8; 79];
    let costas: [u8; 7] = [3, 1, 4, 0, 6, 5, 2];
    let gray_map: [u8; 8] = [0, 1, 3, 2, 5, 6, 4, 7];
    for i in 0..7 {
        tones[i] = costas[i];
        tones[i + 36] = costas[i];
        tones[i + 72] = costas[i];
    }
    for i in 0..29 {
        let val1 = (res.codeword[i * 3] << 2) | (res.codeword[i * 3 + 1] << 1) | res.codeword[i * 3 + 2];
        tones[i + 7] = gray_map[val1 as usize];
        let val2 = (res.codeword[87 + i * 3] << 2) | (res.codeword[87 + i * 3 + 1] << 1) | res.codeword[87 + i * 3 + 2];
        tones[i + 43] = gray_map[val2 as usize];
    }
    tones
}

fn parse_ft8_message(bits: &[u8]) -> Option<packjt77::Ft8Message> {
    if bits.len() < 77 { return None; }
    packjt77::unpack77(&bits[0..77], false)
}

fn is_likely_valid_msg(msg: &str) -> bool {
    let msg = msg.trim().to_uppercase();
    if msg.is_empty() || msg.contains("??") || msg.len() < 3 { return false; }
    
    // 基础字符集
    if !msg.chars().all(|c| c.is_alphanumeric() || " /+-;[]<>:.".contains(c)) { return false; }

    let parts: Vec<&str> = msg.split_whitespace().collect();
    // 允许 2~4 个 parts，4-part 消息包括 "CQ DX CALL GRID" 等定向 CQ
    if parts.len() < 2 || parts.len() > 4 { return false; }
    if parts.len() == 4 && parts[0] != "CQ" { return false; }

    for part in &parts {
        // 斜杠校验
        if part.contains('/') {
            if part.starts_with('/') || part.ends_with('/') || part.contains("//") { return false; }
            if part.matches('/').count() > 2 { return false; }
        }
        // 长度校验
        if part.len() > 13 { return false; }
    }

    true
}

/// 候选点去重：将频率间隔 < freq_tol Hz 且时间相同的候选合并
/// 保留 sync 分数最高的候选。大幅减少 IFFT 计算量。
fn dedup_candidates(candidates: &[SyncCandidate], freq_tol: f32) -> Vec<SyncCandidate> {
    if candidates.is_empty() { return Vec::new(); }
    
    // 已按 sync 降序排列，遍历时标记已被覆盖的
    let mut used = vec![false; candidates.len()];
    let mut result = Vec::with_capacity(candidates.len());
    
    for i in 0..candidates.len() {
        if used[i] { continue; }
        result.push(SyncCandidate {
            freq: candidates[i].freq,
            dt: candidates[i].dt,
            sync: candidates[i].sync,
            noise: candidates[i].noise,
        });
        // 标记与当前候选频率接近且 dt 相同的后续候选为已使用
        for j in (i + 1)..candidates.len() {
            if !used[j] 
                && (candidates[j].freq - candidates[i].freq).abs() < freq_tol
                && (candidates[j].dt - candidates[i].dt).abs() < 0.05 
            {
                used[j] = true;
            }
        }
    }
    result
}