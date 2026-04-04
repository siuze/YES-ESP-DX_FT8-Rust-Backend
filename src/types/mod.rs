use std::sync::{Arc, RwLock, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64};
use std::time::Instant;
use regex::Regex;
use tokio::sync::broadcast;
pub use crate::ft8_codec::Ft8DecodeResult;
pub use crate::ft8_qso::qso_auto_hunter::AutoQsoManager;

pub static GLOBAL_TX: OnceLock<broadcast::Sender<Vec<u8>>> = OnceLock::new();
pub static RADIO_LO_FREQ: AtomicU64 = AtomicU64::new(0);
pub static AUTO_MGR: OnceLock<Arc<Mutex<AutoQsoManager>>> = OnceLock::new();
pub static STATE: OnceLock<Arc<RwLock<AppState>>> = OnceLock::new();
pub static CURRENT_DECODE_TS: AtomicU64 = AtomicU64::new(0);
pub static CURRENT_DT_OFFSET_MS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

// --- 常量配置 ---
pub const SAMPLE_RATE: f32 = 48000.0;
pub const DECIMATION_FACTOR: usize = 4;
pub const DECODE_SAMPLE_RATE: f32 = 12000.0;
pub const WINDOW_SECONDS: usize = 16;
pub const BUFFER_SIZE: usize = (DECODE_SAMPLE_RATE as usize) * WINDOW_SECONDS;
pub const FFT_SIZE: usize = 4096;
pub const TARGET_FREQ: f32 = 3500.0;
pub const TARGET_BINS: usize = ((TARGET_FREQ * FFT_SIZE as f32) / DECODE_SAMPLE_RATE) as usize;
pub const PKT_SIZE_IQ: usize = 1356;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct RustStatus {
    pub tx_count: u32,
    pub decode_count: u32,
    pub uptime_ms: u32,
    pub timestamp_ms: u64,
    pub auto_tx_mode: u8,       // 0:关, 1:单次, 2:自动通联
    pub last_msg: [u8; 24],     // 最近发射
    pub pending_msg: [u8; 24],  // 即将发射
    pub pending_offset: u16,    // 发射频偏
    pub expect_regex: [u8; 48], // 期待消息正则
    pub repeat_count: u8,       // 重复计数
    pub max_repeats: u8,        // 最大重复次数
    pub tx_window_even: u8,     // 1:偶数窗口(0/30)
    pub demod_mode: u8,         // 0:USB, 1:LSB, 2:AM
    pub ft8_decode_on: u8,      // 0:Off, 1:On
    pub audio_on: u8,           // 0:Off, 1:On
    pub usb_bw: u16,
    pub lsb_bw: u16,
    pub am_bw: u16,
}

pub struct AppState {
    pub status: RustStatus,
    pub start_time: Instant,
    pub current_if_hz: u32,
    pub expect_regex_compiled: Option<Regex>,
    pub target_offset: u16,
}

#[repr(C, packed)]
pub struct WSDecodePkt {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub snr: i8,
    pub dt: i16,
    pub freq: i16,
    pub message: [u8; 32],
}

pub struct DecodeBuffer {
    pub data: Vec<i16>,
    pub write_ptr: usize,
}

impl DecodeBuffer {
    pub fn new() -> Self {
        Self {
            data: vec![0; BUFFER_SIZE],
            write_ptr: 0,
        }
    }
    pub fn push(&mut self, sample: i16) {
        self.data[self.write_ptr] = sample;
        self.write_ptr = (self.write_ptr + 1) % BUFFER_SIZE;
    }
    pub fn get_last_16s(&self) -> Vec<i16> {
        let mut out = Vec::with_capacity(BUFFER_SIZE);
        for i in 0..BUFFER_SIZE {
            out.push(self.data[(self.write_ptr + i) % BUFFER_SIZE]);
        }
        out
    }
}
