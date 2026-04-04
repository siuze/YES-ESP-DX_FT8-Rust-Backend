pub mod qso_auto_hunter;   // Mode 3: 持续自动通联猎手 (AutoQsoManager)
pub mod qso_auto_once;    // Mode 2: 单次自动答复驱动
pub mod qso_utils;         // FT8 消息格式工具 (get_next_tx_msg, is_snr, is_grid 等)
pub mod location;
pub mod notion_logger;
pub mod psk_reporter;

pub use qso_utils::*;
