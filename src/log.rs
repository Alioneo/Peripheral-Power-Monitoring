//! 简易日志：仅在开启时写数据目录下的 `rapoo-battery.log`，上限 10MB（超了砍旧留新）。

use crate::sys;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 日志上限：10MB。
const MAX_LOG: u64 = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct Log {
    path: Option<PathBuf>,
}

impl Log {
    pub fn new(enabled: bool) -> Log {
        if !enabled {
            return Log { path: None };
        }
        Log {
            path: Some(crate::config::data_dir().join("rapoo-battery.log")),
        }
    }

    pub fn line(&self, msg: &str) {
        if let Some(path) = &self.path {
            rotate_if_full(path);
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "[{}] {}", sys::now_hms(), msg);
            }
        }
    }
}

/// 超过 10MB 就保留最后 5MB（对齐到行边界）整体替换，避免日志无限长大。
fn rotate_if_full(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_LOG {
        return;
    }
    let Ok(data) = std::fs::read(path) else {
        return;
    };
    let mut start = data.len().saturating_sub((MAX_LOG / 2) as usize);
    if let Some(i) = data[start..].iter().position(|&b| b == b'\n') {
        start += i + 1;
    }
    let body = String::from_utf8_lossy(&data[start..]);
    let _ = crate::config::write_atomic(path, &format!("—— 日志超过 10MB，旧部分已截断 ——\n{body}"));
}
