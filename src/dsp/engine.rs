use num_complex::Complex32;
use crate::types::SAMPLE_RATE;

pub struct SdrEngine {
    pub taps_usb: Vec<Complex32>,
    pub taps_lsb: Vec<Complex32>,
    pub taps_am: Vec<f32>,
    pub history_i: Vec<f32>,
    pub history_q: Vec<f32>,
    pub head: usize,
    // 当前带宽缓存
    pub usb_bw: u16,
    pub lsb_bw: u16,
    pub am_bw: u16,
}

impl SdrEngine {
    pub fn new(taps_count: usize) -> Self {
        let mut engine = Self {
            taps_usb: vec![Complex32::new(0.0, 0.0); taps_count],
            taps_lsb: vec![Complex32::new(0.0, 0.0); taps_count],
            taps_am: vec![0.0; taps_count],
            history_i: vec![0.0; taps_count],
            history_q: vec![0.0; taps_count],
            head: 0,
            usb_bw: 0,
            lsb_bw: 0,
            am_bw: 0,
        };
        engine.update_taps(3000, 3000, 6000);
        engine
    }

    pub fn update_taps(&mut self, usb: u16, lsb: u16, am: u16) {
        if self.usb_bw == usb && self.lsb_bw == lsb && self.am_bw == am {
            return;
        }
        self.usb_bw = usb;
        self.lsb_bw = lsb;
        self.am_bw = am;

        let taps_count = self.taps_am.len();
        let m = (taps_count - 1) as f32;

        // AM 滤波器
        let fc_am = (am as f32 / 2.0) / SAMPLE_RATE;
        for i in 0..taps_count {
            let n = i as f32 - m / 2.0;
            let sinc = if n == 0.0 { 2.0 * fc_am } else { (2.0 * std::f32::consts::PI * fc_am * n).sin() / (std::f32::consts::PI * n) };
            let window = 0.42 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / m).cos() + 0.08 * (4.0 * std::f32::consts::PI * i as f32 / m).cos();
            self.taps_am[i] = sinc * window;
        }

        // USB
        let fc_usb = (usb as f32 / 2.0) / SAMPLE_RATE;
        let shift_usb = 2.0 * std::f32::consts::PI * (usb as f32 / 2.0 / SAMPLE_RATE);
        for i in 0..taps_count {
            let n = i as f32 - m / 2.0;
            let sinc = if n == 0.0 { 2.0 * fc_usb } else { (2.0 * std::f32::consts::PI * fc_usb * n).sin() / (std::f32::consts::PI * n) };
            let window = 0.42 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / m).cos() + 0.08 * (4.0 * std::f32::consts::PI * i as f32 / m).cos();
            self.taps_usb[i] = Complex32::from_polar(sinc * window, shift_usb * n);
        }

        // LSB
        let fc_lsb = (lsb as f32 / 2.0) / SAMPLE_RATE;
        let shift_lsb = 2.0 * std::f32::consts::PI * (lsb as f32 / 2.0 / SAMPLE_RATE);
        for i in 0..taps_count {
            let n = i as f32 - m / 2.0;
            let sinc = if n == 0.0 { 2.0 * fc_lsb } else { (2.0 * std::f32::consts::PI * fc_lsb * n).sin() / (std::f32::consts::PI * n) };
            let window = 0.42 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / m).cos() + 0.08 * (4.0 * std::f32::consts::PI * i as f32 / m).cos();
            self.taps_lsb[i] = Complex32::from_polar(sinc * window, -shift_lsb * n);
        }
    }

    pub fn process(&mut self, i: f32, q: f32, mode: u8) -> (f32, f32) {
        self.history_i[self.head] = i;
        self.history_q[self.head] = q;

        let out = match mode {
            1 => { // LSB
                let mut acc = Complex32::new(0.0, 0.0);
                for k in 0..self.taps_lsb.len() {
                    let idx = (self.head + self.taps_lsb.len() - k) % self.taps_lsb.len();
                    let s = Complex32::new(self.history_i[idx], self.history_q[idx]);
                    acc += s * self.taps_lsb[k];
                }
                (acc.re, -acc.im)
            }
            2 => { // AM
                let mut acc_i = 0.0;
                let mut acc_q = 0.0;
                for k in 0..self.taps_am.len() {
                    let idx = (self.head + self.taps_am.len() - k) % self.taps_am.len();
                    acc_i += self.history_i[idx] * self.taps_am[k];
                    acc_q += self.history_q[idx] * self.taps_am[k];
                }
                ((acc_i * acc_i + acc_q * acc_q).sqrt(), 0.0)
            }
            _ => { // USB
                let mut acc = Complex32::new(0.0, 0.0);
                for k in 0..self.taps_usb.len() {
                    let idx = (self.head + self.taps_usb.len() - k) % self.taps_usb.len();
                    let s = Complex32::new(self.history_i[idx], self.history_q[idx]);
                    acc += s * self.taps_usb[k];
                }
                (acc.re, acc.im)
            }
        };

        self.head = (self.head + 1) % self.taps_am.len();
        out
    }
}
