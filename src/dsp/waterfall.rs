use rustfft::{FftPlanner, num_complex::Complex};
use std::sync::Arc;

pub struct Waterfall {
    pub max_blocks: usize,
    pub num_blocks: usize,
    pub num_bins: usize,
    pub time_osr: usize,
    pub freq_osr: usize,
    pub mag: Vec<u8>,
    pub block_stride: usize,
    pub min_bin: usize,
    pub _max_bin: usize,
    pub _symbol_period: f32,
    pub _protocol: u8,
}

impl Waterfall {
    pub fn new(max_blocks: usize, num_bins: usize, time_osr: usize, freq_osr: usize, min_bin: usize) -> Self {
        let block_stride = time_osr * freq_osr * num_bins;
        Self {
            max_blocks,
            num_blocks: 0,
            num_bins,
            time_osr,
            freq_osr,
            mag: vec![0; max_blocks * block_stride],
            block_stride,
            min_bin,
            _max_bin: min_bin + num_bins,
            _symbol_period: 0.160,
            _protocol: 1, // FT8
        }
    }
}

pub struct Monitor {
    pub block_size: usize,
    pub subblock_size: usize,
    pub nfft: usize,
    pub window: Vec<f32>,
    pub last_frame: Vec<f32>,
    pub wf: Waterfall,
    pub min_bin: usize,
    pub max_bin: usize,
    pub _symbol_period: f32,
    fft: Arc<dyn rustfft::Fft<f32>>,
}

impl Monitor {
    pub fn new(sample_rate: f32, f_min: f32, f_max: f32) -> Self {
        let symbol_period = 0.160;
        let block_size = (sample_rate * symbol_period) as usize;
        let time_osr = 2;
        let freq_osr = 2;
        let subblock_size = block_size / time_osr;
        let nfft = block_size * freq_osr;
        let fft_norm = 2.0 / nfft as f32;

        let mut window = vec![0.0; nfft];
        for i in 0..nfft {
            let x = (std::f32::consts::PI * i as f32 / nfft as f32).sin();
            window[i] = fft_norm * x * x; // Hann window
        }

        let min_bin = (f_min * symbol_period) as usize;
        let max_bin = (f_max * symbol_period) as usize + 1;
        let num_bins = max_bin - min_bin;
        let max_blocks = (15.0 / symbol_period) as usize;

        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(nfft);

        Self {
            block_size,
            subblock_size,
            nfft,
            window,
            last_frame: vec![0.0; nfft],
            wf: Waterfall::new(max_blocks, num_bins, time_osr, freq_osr, min_bin),
            min_bin,
            max_bin,
            _symbol_period: symbol_period,
            fft,
        }
    }

    pub fn process(&mut self, frame: &[f32]) {
        if self.wf.num_blocks >= self.wf.max_blocks {
            return;
        }

        let mut offset = self.wf.num_blocks * self.wf.block_stride;
        let mut frame_pos = 0;

        for _time_sub in 0..self.wf.time_osr {
            // Shift last_frame
            for pos in 0..(self.nfft - self.subblock_size) {
                self.last_frame[pos] = self.last_frame[pos + self.subblock_size];
            }
            for pos in (self.nfft - self.subblock_size)..self.nfft {
                if frame_pos < frame.len() {
                    self.last_frame[pos] = frame[frame_pos];
                    frame_pos += 1;
                } else {
                    self.last_frame[pos] = 0.0;
                }
            }

            // Window and FFT
            let mut buffer: Vec<Complex<f32>> = self.last_frame.iter().enumerate()
                .map(|(i, &x)| Complex::new(x * self.window[i], 0.0))
                .collect();
            
            self.fft.process(&mut buffer);

            for freq_sub in 0..self.wf.freq_osr {
                for bin in self.min_bin..self.max_bin {
                    let src_bin = bin * self.wf.freq_osr + freq_sub;
                    if src_bin >= buffer.len() {
                        offset += 1;
                        continue;
                    }
                    let mag2 = buffer[src_bin].norm_sqr();
                    let db = 10.0 * (1e-12 + mag2).log10();
                    
                    let scaled = (2.0 * db + 240.0) as i32;
                    self.wf.mag[offset] = scaled.clamp(0, 255) as u8;
                    offset += 1;
                }
            }
        }
        self.wf.num_blocks += 1;
    }
}
