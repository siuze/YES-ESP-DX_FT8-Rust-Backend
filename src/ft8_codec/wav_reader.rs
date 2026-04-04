//! WAV 文件读取模块

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// WAV 文件头结构
#[derive(Debug, Clone)]
pub struct WavHeader {
    pub file_size: u32,
    pub audio_format: u16,
    pub num_channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub data_size: u32,
}

impl WavHeader {
    /// 从文件中读取 WAV 头
    pub fn read_from_file(filename: &str) -> Result<(Self, Vec<i16>), String> {
        let mut file = File::open(filename).map_err(|e| format!("无法打开文件：{}", e))?;
        
        // 读取 RIFF header (12 bytes)
        let mut riff = [0u8; 4];
        file.read_exact(&mut riff).map_err(|e| format!("读取 RIFF 标记失败：{}", e))?;
        if &riff != b"RIFF" {
            return Err(format!("无效的 RIFF 标记"));
        }
        
        let mut file_size_bytes = [0u8; 4];
        file.read_exact(&mut file_size_bytes).map_err(|e| e.to_string())?;
        let _file_size = u32::from_le_bytes(file_size_bytes);
        
        let mut wave = [0u8; 4];
        file.read_exact(&mut wave).map_err(|e| e.to_string())?;
        if &wave != b"WAVE" {
            return Err("无效的 WAVE 标记".to_string());
        }
        
        // 查找 fmt 和 data chunks
        let mut audio_format = 0u16;
        let mut num_channels = 0u16;
        let mut sample_rate = 0u32;
        let mut bits_per_sample = 0u16;
        let data_size;
        
        loop {
            // 读取 chunk ID
            let mut chunk_id = [0u8; 4];
            if file.read_exact(&mut chunk_id).is_err() {
                return Err("找不到 data chunk".to_string());
            }
            
            // 读取 chunk size
            let mut chunk_size_bytes = [0u8; 4];
            file.read_exact(&mut chunk_size_bytes).map_err(|e| e.to_string())?;
            let chunk_size = u32::from_le_bytes(chunk_size_bytes);
            
            let chunk_id_str = String::from_utf8_lossy(&chunk_id);
            
            if chunk_id_str == "fmt " {
                // 读取 fmt chunk (至少 16 字节)
                let mut fmt_data = vec![0u8; chunk_size as usize];
                file.read_exact(&mut fmt_data).map_err(|e| e.to_string())?;
                
                audio_format = u16::from_le_bytes([fmt_data[0], fmt_data[1]]);
                num_channels = u16::from_le_bytes([fmt_data[2], fmt_data[3]]);
                sample_rate = u32::from_le_bytes([fmt_data[4], fmt_data[5], fmt_data[6], fmt_data[7]]);
                bits_per_sample = u16::from_le_bytes([fmt_data[14], fmt_data[15]]);
            } else if chunk_id_str == "data" {
                data_size = chunk_size;
                break;
            } else {
                // 跳过未知的 chunk
                file.seek(SeekFrom::Current(chunk_size as i64)).map_err(|e| e.to_string())?;
            }
        }
        
        let header = WavHeader {
            file_size: 0,
            audio_format,
            num_channels,
            sample_rate,
            bits_per_sample,
            data_size,
        };
        
        // 验证 bits_per_sample
        if bits_per_sample == 0 {
            return Err(format!("无效的位深度 (0)"));
        }
        
        // 读取音频数据
        let bytes_per_sample = (bits_per_sample / 8) as usize;
        let num_samples = data_size as usize / bytes_per_sample;
        let mut samples = vec![0i16; num_samples];
        
        for sample in &mut samples {
            let mut bytes = [0u8; 2];
            file.read_exact(&mut bytes).map_err(|e| format!("读取样本数据失败：{}", e))?;
            *sample = i16::from_le_bytes(bytes);
        }
        
        Ok((header, samples))
    }
    
    /// 打印 WAV 文件信息
    pub fn print_info(&self) {
        println!("WAV 文件信息:");
        println!("  采样率：{} Hz", self.sample_rate);
        println!("  声道数：{}", self.num_channels);
        println!("  位深度：{}", self.bits_per_sample);
        println!("  数据大小：{} 字节", self.data_size);
    }
}
