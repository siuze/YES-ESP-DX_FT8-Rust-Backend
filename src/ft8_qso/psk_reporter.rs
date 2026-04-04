use std::collections::HashMap;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use chrono::Utc;

use crate::config;

// PSK Reporter 服务器地址：从 config 统一管理

#[derive(Debug, Clone)]
pub struct PskSpot {
    pub callsign: String,
    pub frequency_hz: u64,
    pub snr: i8,
    pub dt_ms: i16, 
    pub sender_grid: String,
}

pub struct PskReporter {
    rx: mpsc::Receiver<PskSpot>,
    udp_socket: UdpSocket,
    sequence_number: u32,
    spot_buffer: HashMap<String, PskSpot>,
}

impl PskReporter {
    pub async fn init(rx: mpsc::Receiver<PskSpot>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let udp_socket = UdpSocket::bind("0.0.0.0:0").await?;
        Ok(Self {
            rx,
            udp_socket,
            sequence_number: 0,
            spot_buffer: HashMap::new(),
        })
    }

    pub async fn run(mut self) {
        println!("[PSK] 直连模式已启动，目标域名: {}", config::PSK_REPORTER_HOST);

        let mut interval = time::interval(Duration::from_secs(300));
        loop {
            tokio::select! {
                Some(spot) = self.rx.recv() => {
                    self.spot_buffer.insert(spot.callsign.clone(), spot);
                }
                _ = interval.tick() => {
                    if !self.spot_buffer.is_empty() {
                        if let Err(e) = self.send_report().await {
                            eprintln!("[PSK] 直连发送失败 (DNS解析或网络问题): {}", e);
                        }
                    }
                }
            }
        }
    }

    async fn send_report(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut ipfix_data = Vec::with_capacity(1024);
        let now_unix = Utc::now().timestamp() as u32;

        // --- 1. 构造 IPFIX 报文头 ---
        ipfix_data.extend_from_slice(&10u16.to_be_bytes()); 
        ipfix_data.extend_from_slice(&0u16.to_be_bytes()); // 占位总长度
        ipfix_data.extend_from_slice(&now_unix.to_be_bytes());
        ipfix_data.extend_from_slice(&self.sequence_number.to_be_bytes());
        ipfix_data.extend_from_slice(&config::PSK_FIXED_RANDOM_ID.to_be_bytes());

        // --- 2. 插入 Record Format Descriptors (模板) ---
        // Template 9992 (Receiver Info, 5字段) - 严格对齐文档
        ipfix_data.extend_from_slice(&[
            0x00, 0x03, 0x00, 0x34, 0x99, 0x92, 0x00, 0x05, 0x00, 0x01,
            0x80, 0x02, 0xFF, 0xFF, 0x00, 0x00, 0x76, 0x8F, 
            0x80, 0x04, 0xFF, 0xFF, 0x00, 0x00, 0x76, 0x8F, 
            0x80, 0x08, 0xFF, 0xFF, 0x00, 0x00, 0x76, 0x8F, 
            0x80, 0x09, 0xFF, 0xFF, 0x00, 0x00, 0x76, 0x8F, 
            0x80, 0x0D, 0xFF, 0xFF, 0x00, 0x00, 0x76, 0x8F, 
            0x00, 0x00
        ]);
        
        // Template 9993 (Sender Info, 8字段) - 严格对齐文档中 00 02 00 44... 这一段
        ipfix_data.extend_from_slice(&[
            0x00, 0x02, 0x00, 0x44, 0x99, 0x93, 0x00, 0x08, 
            0x80, 0x01, 0xFF, 0xFF, 0x00, 0x00, 0x76, 0x8F, // senderCallsign
            0x80, 0x05, 0x00, 0x04, 0x00, 0x00, 0x76, 0x8F, // frequency
            0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x76, 0x8F, // snr
            0x80, 0x07, 0x00, 0x01, 0x00, 0x00, 0x76, 0x8F, // imd
            0x80, 0x0A, 0xFF, 0xFF, 0x00, 0x00, 0x76, 0x8F, // mode
            0x80, 0x0B, 0x00, 0x01, 0x00, 0x00, 0x76, 0x8F, // infoSource
            0x80, 0x03, 0xFF, 0xFF, 0x00, 0x00, 0x76, 0x8F, // senderLocator
            0x00, 0x96, 0x00, 0x04,                         // flowStartSeconds
        ]);

        // --- 3. 插入 Data ---
        // Data 9992 (接收端数据)
        let mut r_data = Vec::new();
        Self::enc_str(&mut r_data, config::MY_CALL);
        Self::enc_str(&mut r_data, config::MY_GRID_DETAIL);
        Self::enc_str(&mut r_data, config::SOFTWARE);
        Self::enc_str(&mut r_data, config::ANTENNA);
        Self::enc_str(&mut r_data, config::RIG);
        Self::pad4(&mut r_data);
        ipfix_data.extend_from_slice(&0x9992u16.to_be_bytes());
        ipfix_data.extend_from_slice(&((r_data.len() + 4) as u16).to_be_bytes());
        ipfix_data.extend_from_slice(&r_data);

        // Data 9993 (发送端/Spot 数据)
        let mut s_data = Vec::new();
        let count = self.spot_buffer.len() as u32;
        for (_, spot) in &self.spot_buffer {
            Self::enc_str(&mut s_data, &spot.callsign);
            s_data.extend_from_slice(&(spot.frequency_hz as u32).to_be_bytes()); // Hz
            s_data.push(spot.snr as u8); // SNR (-128 到 127，底层转为无符号字节发送)
            s_data.push(127);            // IMD: FT8通常没有此数据，发 127 代表未知
            Self::enc_str(&mut s_data, "FT8");
            s_data.push(1);              // infoSource: 1 = Automatically Extracted
            Self::enc_str(&mut s_data, &spot.sender_grid);
            s_data.extend_from_slice(&now_unix.to_be_bytes());
        }
        Self::pad4(&mut s_data);
        ipfix_data.extend_from_slice(&0x9993u16.to_be_bytes());
        ipfix_data.extend_from_slice(&((s_data.len() + 4) as u16).to_be_bytes());
        ipfix_data.extend_from_slice(&s_data);

        // 回填总长度
        let total_len = ipfix_data.len() as u16;
        ipfix_data[2..4].copy_from_slice(&total_len.to_be_bytes());

        // --- 4. 自动 DNS 解析并发送给 PSK Reporter ---
        // tokio 会在这里自动将域名转换为 IP 地址并发送 UDP
        // --- 4. 自动 DNS 解析，强制过滤并使用 IPv4 地址 ---
        let mut target_ipv4 = None;
        // 查找该域名对应的所有 IP 地址
        for addr in tokio::net::lookup_host(config::PSK_REPORTER_HOST).await? {
            if addr.is_ipv4() {
                target_ipv4 = Some(addr);
                break; // 找到第一个 IPv4 地址就停止
            }
        }

        // 如果没有找到 IPv4，则报错返回
        let target_addr = target_ipv4.ok_or(format!("DNS 解析失败：未找到 {} 的 IPv4 地址", config::PSK_REPORTER_HOST))?;

        // 发送 UDP 数据包
        self.udp_socket.send_to(&ipfix_data, target_addr).await?;
        println!("[PSK] 成功上报 {} 条记录至 {} ({}), 序列号: {}", 
            count, config::PSK_REPORTER_HOST, target_addr.ip(), self.sequence_number);
        println!("[PSK] 成功通过域名解析上报 {} 条记录至 {}, 序列号: {}", count, config::PSK_REPORTER_HOST, self.sequence_number);
        
        self.sequence_number += count;
        self.spot_buffer.clear();

        Ok(())
    }

    fn enc_str(buf: &mut Vec<u8>, s: &str) {
        let b = s.as_bytes();
        buf.push(b.len() as u8);
        buf.extend_from_slice(b);
    }
    
    fn pad4(buf: &mut Vec<u8>) {
        while (buf.len() + 4) % 4 != 0 { buf.push(0); }
    }
}