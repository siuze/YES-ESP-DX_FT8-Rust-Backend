use tokio::net::UdpSocket;
use crate::types::GLOBAL_TX;

/// 启动无线电数据桥接任务
pub fn spawn_radio_bridge_to_pc_task() {
    tokio::spawn(async move {
        // 绑定 1532 端口接收电台上行数据
        let socket = UdpSocket::bind("0.0.0.0:1532").await.unwrap();
        let mut buf = [0u8; 2048];
        loop {
            if let Ok((n, addr)) = socket.recv_from(&mut buf).await {
                // 1. IP 自动发现逻辑
                {
                    let state_arc = crate::types::STATE.get().unwrap();
                    let mut s = state_arc.write().unwrap();
                    if s.radio_ip.is_none() {
                        let discovered_ip = addr.ip().to_string();
                        s.radio_ip = Some(discovered_ip.clone());
                        crate::utils::log_to_pc(&format!("🌐 发现无线电台 IP: {}", discovered_ip));
                        
                        // 发现 IP 后，立即获取本机内网 IP 并下发给电台
                        if let Some(local_ip) = crate::radio_ctrl::get_local_ip() {
                            crate::utils::log_to_pc(&format!("📤 正在向电台同步上位机 IP: {}", local_ip));
                            let _ = crate::radio_ctrl::set_pc_ip(&discovered_ip, &local_ip);
                        }
                    }
                }

                // 2. 将原始二进制数据包封装并广播至所有 WebSocket 客户端
                let mut p = vec![0x01]; 
                p.extend_from_slice(&buf[..n]);
                if let Some(tx) = GLOBAL_TX.get() {
                    let _ = tx.send(p);
                }

                // 3. 解析电台发回的关键状态、日志与回显信息 (交由统一解析模块)
                crate::radio_ctrl::parser::parse_radio_packet(&buf[..n]);
            }
        }
    });
}
