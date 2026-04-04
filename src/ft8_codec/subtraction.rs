use std::f32::consts::PI;
use num_complex::Complex32;
use rustfft::FftPlanner;
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref PULSE_CACHE: Mutex<Option<Vec<f32>>> = Mutex::new(None);
}

/// 高精度 erf 函数近似
#[inline]
fn erf(x: f32) -> f32 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let y = 1.0 - (((((a5 * t + a4) * t + a3) * t + a2) * t + a1) * t) * (-(x * x)).exp();
    if x >= 0.0 { y } else { -y }
}

/// GFSK 脉冲响应
#[inline]
fn gfsk_pulse(b: f32, t: f32) -> f32 {
    let c = PI * (2.0 / 2.0f32.ln()).sqrt();
    0.5 * (erf(c * b * (t + 0.5)) - erf(c * b * (t - 0.5)))
}

/// 生成 FT8 信号波形
pub fn gen_ft8wave(itone: &[u8], f0: f32, fsample: f32, nsps: usize) -> Vec<Complex32> {
    let nsym = itone.len();
    let bt = 2.0;
    let hmod = 1.0;
    let dt = 1.0 / fsample;
    let nwave = nsym * nsps;
    
    // 1. 获取或计算 GFSK 平滑脉冲
    let pulse = {
        let mut cache = PULSE_CACHE.lock().unwrap();
        if cache.is_none() {
            let mut p = vec![0.0f32; 3 * nsps];
            for i in 0..3 * nsps {
                let tt = (i as f32 - 1.5 * nsps as f32) / nsps as f32;
                p[i] = gfsk_pulse(bt, tt);
            }
            *cache = Some(p);
        }
        cache.as_ref().unwrap().clone()
    };
    
    // 2. 计算相位导数 (dphi)
    let mut dphi = vec![0.0f32; (nsym + 2) * nsps];
    let dphi_peak = 2.0 * PI * hmod / nsps as f32;
    
    for j in 0..nsym {
        let tone_val = itone[j] as f32;
        if tone_val == 0.0 { continue; }
        let start = j * nsps;
        let factor = dphi_peak * tone_val;
        for i in 0..3 * nsps {
            if start + i < dphi.len() {
                dphi[start + i] += factor * pulse[i];
            }
        }
    }
    
    // 补偿边界符号
    let factor0 = dphi_peak * itone[0] as f32;
    for i in 0..2 * nsps {
        dphi[i] += factor0 * pulse[nsps + i];
    }
    let factor_last = dphi_peak * itone[nsym - 1] as f32;
    for i in 0..2 * nsps {
        let idx = nsym * nsps + i;
        if idx < dphi.len() {
            dphi[idx] += factor_last * pulse[i];
        }
    }
    
    // 3. 生成音频波形
    let mut cwave = vec![Complex32::new(0.0, 0.0); nwave];
    let freq_shift = 2.0 * PI * f0 * dt;
    let mut phi = 0.0f32;
    
    for j in 0..nwave {
        let (s, c) = phi.sin_cos();
        cwave[j] = Complex32::new(c, s);
        phi = (phi + dphi[j + nsps] + freq_shift) % (2.0 * PI);
    }
    
    // 4. 应用包络整形 (Ramp up/down)
    let nramp = nsps / 8;
    for i in 0..nramp {
        let factor = (1.0 - (2.0 * PI * i as f32 / (2.0 * nramp as f32)).cos()) / 2.0;
        cwave[i] *= factor;
        cwave[nwave - 1 - i] *= factor;
    }
    
    cwave
}

pub fn subtract_ft8(
    dd: &mut [f32],
    itone: &[u8],
    f0: f32,
    dt_offset: f32,
    fsample: f32,
    nsps: usize,
    planner: &mut FftPlanner<f32>
) {
    let nframe = itone.len() * nsps;
    let nmax = dd.len();
    let nfilt = 2000;
    let nfft = next_power_of_2(nframe + nfilt);
    
    // 1. 生成参考波形
    let cref = gen_ft8wave(itone, f0, fsample, nsps);
    
    // 2. 提取并计算复幅度 (dd * conj(cref))
    let nstart = ((dt_offset + 0.5) * fsample).round() as isize;
    let mut cfilt = vec![Complex32::new(0.0, 0.0); nfft];
    for i in 0..nframe {
        let j = nstart + i as isize;
        if j >= 0 && j < nmax as isize {
            cfilt[i] = Complex32::new(dd[j as usize], 0.0) * cref[i].conj();
        }
    }
    
    // 3. 构建频率响应 (LPF)
    let mut filter_h = vec![Complex32::new(0.0, 0.0); nfft];
    let mut sumw = 0.0f32;
    for j in -(nfilt as i32) / 2..=(nfilt as i32) / 2 {
        let val = (PI * j as f32 / nfilt as f32).cos().powi(2);
        let idx = ((j + nfft as i32) % nfft as i32) as usize;
        filter_h[idx] = Complex32::new(val, 0.0);
        sumw += val;
    }
    for val in filter_h.iter_mut() { *val /= sumw; }
    
    let fft_plan = planner.plan_fft_forward(nfft);
    fft_plan.process(&mut filter_h);
    
    // 4. 执行滤波
    fft_plan.process(&mut cfilt);
    for i in 0..nfft { cfilt[i] *= filter_h[i]; }
    
    let ifft_plan = planner.plan_fft_inverse(nfft);
    ifft_plan.process(&mut cfilt);
    let fac = 1.0 / nfft as f32;
    
    // 5. 减去信号
    for i in 0..nframe {
        let j = nstart + i as isize;
        if j >= 0 && j < nmax as isize {
            dd[j as usize] -= 2.0 * (cfilt[i] * cref[i] * fac).re;
        }
    }
}

fn next_power_of_2(n: usize) -> usize {
    let mut p = 1;
    while p < n { p <<= 1; }
    p
}
