use std::net::UdpSocket;
use std::io;

pub mod parser; // 新增：数据包解析子模块

pub const RADIO_CTRL_PORT: u16 = 1432;
pub const RADIO_RECV_PORT: u16 = 1532;
pub const MAGIC: u16 = 0x2DE3; // Little-endian wire format: [0xE3, 0x2D]
pub const VERSION: u8 = 0x01;

// 指令类型定义 (基于官方文档)
pub const CMD_QUERY_STATUS: u8 = 1;
pub const CMD_REBOOT: u8 = 2;
pub const CMD_OTA: u8 = 3;
pub const CMD_LOG: u8 = 5;
pub const CMD_SET_PC_IP: u8 = 6;
pub const CMD_SET_LO_FREQ: u8 = 20;
pub const CMD_SET_BASE_FREQ: u8 = 21;
pub const CMD_SWITCH_BPF: u8 = 22;
pub const CMD_SWITCH_LPF: u8 = 23;
pub const CMD_SWITCH_RX_STBY: u8 = 24;
pub const CMD_SET_POWER: u8 = 25;
pub const CMD_SET_SWR_PROT: u8 = 26;
pub const CMD_SET_CURR_PROT: u8 = 27;
pub const CMD_SEND_FT8: u8 = 28;
pub const CMD_TONE_TEST: u8 = 40;
pub const CMD_SET_STG_FREQ: u8 = 41;

// 响应/异步上报类型
pub const MSG_RESULT: u8 = 50;
pub const MSG_FT8_ECHO: u8 = 51;

/// 发送通用控制包到电台
/// 协议头: MAGIC(2B) + VER(1B) + TYPE(1B) + PAYLOAD
pub fn send_command(target_ip: &str, cmd_type: u8, payload: &[u8]) -> io::Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;
    
    let mut packet = Vec::with_capacity(4 + payload.len());
    packet.extend_from_slice(&MAGIC.to_le_bytes()); 
    packet.push(VERSION);
    packet.push(cmd_type);
    packet.extend_from_slice(payload);

    let target = format!("{}:{}", target_ip, RADIO_CTRL_PORT);
    socket.send_to(&packet, target)?;
    
    Ok(())
}

/// 执行 CMD 1: 查询设备状态 (心跳包)
pub fn query_status(target_ip: &str) -> io::Result<()> {
    send_command(target_ip, CMD_QUERY_STATUS, &[])
}

/// 执行 CMD 6: 设置上位机 IP 地址
pub fn set_pc_ip(target_ip: &str, pc_ip: &str) -> io::Result<()> {
    let addr: std::net::Ipv4Addr = pc_ip.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid IP"))?;
    let payload = addr.octets(); 
    send_command(target_ip, CMD_SET_PC_IP, &payload)
}

/// 执行 CMD 40: 发射单音测试信号
pub fn send_tone_test(target_ip: &str, freq_hz: u64, duration_ms: u32) -> io::Result<()> {
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(&(freq_hz * 100).to_le_bytes()); // 0.01Hz
    payload.extend_from_slice(&duration_ms.to_le_bytes());
    send_command(target_ip, CMD_TONE_TEST, &payload)
}

/// 执行 CMD 41: 设置信号发生器频率 (测试接收用)
pub fn set_stg_frequency(target_ip: &str, freq_hz: u64) -> io::Result<()> {
    let val = freq_hz * 100;
    let payload = val.to_le_bytes();
    send_command(target_ip, CMD_SET_STG_FREQ, &payload)
}

/// 执行 CMD 20: 设置 LO 频率 (单位 0.01Hz)
pub fn set_lo_frequency(target_ip: &str, freq_hz: u64) -> io::Result<()> {
    let val = freq_hz * 100;
    let payload = val.to_le_bytes();
    send_command(target_ip, CMD_SET_LO_FREQ, &payload)
}

/// 执行 CMD 21: 设置发射基频 (单位 0.01Hz)
pub fn set_base_frequency(target_ip: &str, freq_hz: u64) -> io::Result<()> {
    let val = freq_hz * 100;
    let payload = val.to_le_bytes();
    send_command(target_ip, CMD_SET_BASE_FREQ, &payload)
}

/// 执行 CMD 2: 重启设备
pub fn reboot(target_ip: &str) -> io::Result<()> {
    send_command(target_ip, CMD_REBOOT, &[])
}

/// 执行 CMD 3: 触发固件更新
pub fn trigger_ota(target_ip: &str, url: &str) -> io::Result<()> {
    send_command(target_ip, CMD_OTA, url.as_bytes())
}

/// 执行 CMD 22: 切换带通滤波器 (0/1/2/3/4)
pub fn switch_bpf(target_ip: &str, bpf: u8) -> io::Result<()> {
    send_command(target_ip, CMD_SWITCH_BPF, &[bpf])
}

/// 执行 CMD 23: 切换低通滤波器 (0/1/2/3)
pub fn switch_lpf(target_ip: &str, lpf: u8) -> io::Result<()> {
    send_command(target_ip, CMD_SWITCH_LPF, &[lpf])
}

/// 执行 CMD 24: 切换接收/待机模式 (0:待机, 1:接收)
pub fn set_rx_mode(target_ip: &str, active: bool) -> io::Result<()> {
    send_command(target_ip, CMD_SWITCH_RX_STBY, &[if active { 1 } else { 0 }])
}

/// 执行 CMD 25: 设置发射功率 PWM 占空比 (最大 2800)
pub fn set_tx_power(target_ip: &str, duty: u16) -> io::Result<()> {
    send_command(target_ip, CMD_SET_POWER, &duty.to_le_bytes())
}

/// 执行 CMD 26: 设置驻波保护阈值 (25 代表 2.5)
pub fn set_swr_protection(target_ip: &str, threshold: u16) -> io::Result<()> {
    send_command(target_ip, CMD_SET_SWR_PROT, &threshold.to_le_bytes())
}

/// 执行 CMD 27: 设置发射电流保护阈值 (mA)
pub fn set_current_protection(target_ip: &str, threshold_ma: u16) -> io::Result<()> {
    send_command(target_ip, CMD_SET_CURR_PROT, &threshold_ma.to_le_bytes())
}

/// 执行 CMD 28: 发射 FT8 消息 (包含音调和文本)
pub fn send_ft8_transmit(target_ip: &str, msg: &str, offset: u16) -> io::Result<()> {
    use std::ffi::CString;
    use crate::ft8_codec::encode_ft8_symbols;

    let mut symbols = [0i32; 79];
    let c_msg = CString::new(msg).unwrap();
    
    unsafe {
         encode_ft8_symbols(c_msg.as_ptr(), symbols.as_mut_ptr());
    }

    // 构建 Payload: OFFSET(2B) + SYMBOLS(79B) + TEXT(最多24B)
    let mut payload = Vec::with_capacity(2 + 79 + msg.len().min(24));
    payload.extend_from_slice(&offset.to_le_bytes());
    
    for &s in symbols.iter() {
        payload.push(s as u8);
    }
    
    let msg_bytes = msg.as_bytes();
    let text_len = msg_bytes.len().min(24);
    payload.extend_from_slice(&msg_bytes[..text_len]);

    send_command(target_ip, CMD_SEND_FT8, &payload)
}

/// 获取本机在局域网中的 IP 地址 (简单实用方案)
pub fn get_local_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip().to_string())
}
