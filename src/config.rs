//! 设置持久化：程序同目录的 `rapoo-battery.ini`。
//!
//! 文本键值格式，零依赖；改动由设置页写入，启动时读回。
//! 开机自启用当前用户注册表 Run 键（HKCU），不需要管理员权限。

use crate::log::Log;
use crate::paint::ThemePref;
use crate::sys;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

/// 会随使用增长/变化的数据文件（用于旧目录迁移清单）。
const DATA_FILES: [&str; 5] = [
    "rapoo-battery.ini",
    "rapoo-battery.log",
    "rapoo-battery.state",
    "rapoo-battery.hist",
    "rapoo-battery.charge",
];

/// 数据目录：`%LOCALAPPDATA%\RapooBattery`（装进 Program Files 也可写）。
/// 首次运行时把 exe 旁的旧数据复制过来（不删原件）；取不到 LOCALAPPDATA 时退回 exe 目录。
pub fn data_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let Some(root) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else {
        return exe_dir.unwrap_or_else(|| PathBuf::from("."));
    };
    let dir = root.join("RapooBattery");
    if std::fs::create_dir_all(&dir).is_err() {
        return exe_dir.unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(src) = exe_dir {
        if src != dir {
            for name in DATA_FILES {
                let (from, to) = (src.join(name), dir.join(name));
                if !to.exists() && from.exists() {
                    let _ = std::fs::copy(&from, &to);
                }
            }
        }
    }
    dir
}

/// 原子落盘：先写同目录 `.tmp`，再 MoveFileEx 整体替换，避免写一半留下坏文件。
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
    let src = sys::wide(&tmp.to_string_lossy());
    let dst = sys::wide(&path.to_string_lossy());
    let ok = unsafe {
        sys::MoveFileExW(
            src.as_ptr(),
            dst.as_ptr(),
            sys::MOVEFILE_REPLACE_EXISTING | sys::MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("替换 {} 失败：{err}", path.display()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub autostart: bool,
    pub tray: bool,
    pub threshold: u8,
    pub notify: bool,
    /// 深/浅色主题策略；默认跟随系统。
    pub theme: ThemePref,
    /// 设备显示名：设置页可改，存 ini 做唯一真源（WebView2 缓存不可靠）。
    pub kb_name: String,
    pub ms_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            autostart: false,
            tray: false,
            threshold: 20,
            notify: true,
            theme: ThemePref::Auto,
            kb_name: "键盘".to_string(),
            ms_name: "鼠标".to_string(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("rapoo-battery.ini")
    }

    /// 读取配置，并额外返回“文件里是否明确写了 autostart”。
    ///
    /// 只有明确写了 `autostart=0` 才代表用户要求关掉自启；文件缺失/该键缺失时
    /// 不能据此去删注册表里的启动项。
    pub fn load_ex() -> (Config, bool) {
        let path = Self::path();
        match std::fs::read(&path) {
            Ok(bytes) => Config::parse_ex(&String::from_utf8_lossy(&bytes)),
            Err(_) => (Config::default(), false),
        }
    }

    /// 解析配置文本，并返回 autostart 是否被显式声明。
    pub fn parse_ex(text: &str) -> (Config, bool) {
        // 记事本保存 UTF-8 会带 BOM：不剥掉的话首行键名会变成 "\u{feff}autostart"，
        // 既读不到自启设置，又会被当成“未声明”而误删注册表启动项。
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut cfg = Config::default();
        let mut autostart_explicit = false;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "autostart" => {
                    cfg.autostart = parse_bool(value);
                    autostart_explicit = true;
                }
                "tray" => cfg.tray = parse_bool(value),
                "threshold" => {
                    if let Ok(v) = value.parse::<u8>() {
                        cfg.threshold = v.clamp(5, 60);
                    }
                }
                "notify" => cfg.notify = parse_bool(value),
                "kb_name" => cfg.kb_name = clamp_name(value),
                "ms_name" => cfg.ms_name = clamp_name(value),
                "theme" => {
                    cfg.theme = if value.eq_ignore_ascii_case("dark") {
                        ThemePref::Dark
                    } else if value.eq_ignore_ascii_case("light") {
                        ThemePref::Light
                    } else {
                        ThemePref::Auto
                    }
                }
                _ => {}
            }
        }
        (cfg, autostart_explicit)
    }

    pub fn to_text(&self) -> String {
        format!(
            "# 雷柏键鼠电量入口配置（设置页自动生成）\n\
             autostart={}\n\
             tray={}\n\
             threshold={}\n\
             notify={}\n\
             theme={}\n\
             kb_name={}\n\
             ms_name={}\n",
            self.autostart as u8,
            self.tray as u8,
            self.threshold,
            self.notify as u8,
            self.theme.as_str(),
            self.kb_name,
            self.ms_name
        )
    }

    /// 原子落盘，见 `write_atomic`。
    pub fn save(&self) -> Result<(), String> {
        write_atomic(&Self::path(), &self.to_text())
    }
}

fn parse_bool(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

/// 设备名清洗：去掉控制字符和会破坏 ini/JSON 的引号、反斜杠，最多 14 个字符。
pub fn clamp_name(v: &str) -> String {
    v.chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .take(14)
        .collect()
}

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "RapooBattery";

/// 写入 / 移除当前用户启动项（`HKCU\...\Run`），不需要管理员权限。
pub fn apply_autostart(enabled: bool) -> Result<(), String> {
    let sub = sys::wide(RUN_SUBKEY);
    let name = sys::wide(RUN_NAME);
    unsafe {
        if enabled {
            let exe = std::env::current_exe().map_err(|e| format!("取程序路径失败：{e}"))?;
            let command = format!("\"{}\"", exe.display());
            let data = sys::wide(&command);
            let rc = sys::RegSetKeyValueW(
                sys::HKEY_CURRENT_USER,
                sub.as_ptr(),
                name.as_ptr(),
                sys::REG_SZ,
                data.as_ptr() as *const c_void,
                (data.len() * 2) as u32,
            );
            if rc != 0 {
                return Err(format!("写启动项失败：错误码 {rc}"));
            }
        } else {
            let rc = sys::RegDeleteKeyValueW(sys::HKEY_CURRENT_USER, sub.as_ptr(), name.as_ptr());
            if rc != 0 && rc != sys::ERROR_FILE_NOT_FOUND {
                return Err(format!("删启动项失败：错误码 {rc}"));
            }
        }
    }
    Ok(())
}

/// 启动时按配置同步一次启动项，并写日志（便于排查）。
///
/// `autostart_explicit` 表示 ini 里确实写了 autostart 键。只有明确要求关闭时才删
/// 注册表项；文件/键缺失时保持注册表现状，不因解析失败而静默关掉自启。
pub fn sync_autostart(cfg: &Config, autostart_explicit: bool, log: &Log) {
    if !cfg.autostart && !autostart_explicit {
        log.line(&format!(
            "开机自启：配置未声明 autostart，保持注册表现状（HKCU\\{RUN_SUBKEY}\\{RUN_NAME}）"
        ));
        return;
    }
    match apply_autostart(cfg.autostart) {
        Ok(()) => log.line(&format!(
            "开机自启：{}（HKCU\\{RUN_SUBKEY}\\{RUN_NAME}）",
            if cfg.autostart {
                "已启用"
            } else {
                "已关闭"
            }
        )),
        Err(e) => log.line(&format!("开机自启同步失败：{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        Config::parse_ex(text).0
    }

    #[test]
    fn parse_empty_is_default() {
        assert_eq!(parse(""), Config::default());
        assert_eq!(parse("# 注释\n\n; 也是注释\n"), Config::default());
    }

    #[test]
    fn parse_overrides_and_tolerates_junk() {
        let cfg = parse(
            "  autostart = TRUE \ntray=on\nthreshold=35\nnotify=false\ntheme=LIGHT\nfoo=bar\nbroken-line\n",
        );
        assert!(cfg.autostart);
        assert!(cfg.tray);
        assert_eq!(cfg.threshold, 35);
        assert!(!cfg.notify);
        assert_eq!(cfg.theme, ThemePref::Light);
    }

    #[test]
    fn parse_clamps_threshold_and_keeps_default_on_garbage() {
        assert_eq!(parse("threshold=0\n").threshold, 5);
        assert_eq!(parse("threshold=200\n").threshold, 60);
        assert_eq!(parse("threshold=abc\n").threshold, 20);
    }

    #[test]
    fn bom_does_not_hide_autostart() {
        let (cfg, explicit) = Config::parse_ex("\u{feff}autostart=1\ntray=1\n");
        assert!(cfg.autostart, "带 BOM 时首行 autostart 仍要生效");
        assert!(cfg.tray);
        assert!(explicit, "明确写了 autostart 才算声明");
        let (_, missing) = Config::parse_ex("tray=1\n");
        assert!(!missing, "没写 autostart 时不能当作“明确关闭”");
    }

    #[test]
    fn text_roundtrip() {
        let cfg = Config {
            autostart: true,
            tray: false,
            threshold: 55,
            notify: false,
            theme: ThemePref::Dark,
            kb_name: "我的键盘".to_string(),
            ms_name: "Pro Mouse".to_string(),
        };
        assert_eq!(parse(&cfg.to_text()), cfg);
    }

    #[test]
    fn name_clamp_and_roundtrip() {
        // 引号/反斜杠/换行会被剔除，超长截到 14 字符。
        let (cfg, _) = Config::parse_ex("kb_name=AB\"C\\D\nEF\nms_name=12345678901234567890\n");
        assert_eq!(cfg.kb_name, "ABCD");
        assert_eq!(cfg.ms_name, "12345678901234");
        // 没写名字时用默认中文名。
        let (d, _) = Config::parse_ex("tray=1\n");
        assert_eq!(d.kb_name, "键盘");
        assert_eq!(d.ms_name, "鼠标");
    }
}

pub fn describe(cfg: &Config) -> String {
    format!(
        "自启={} 托盘={} 阈值={} 提醒={} 主题={}",
        cfg.autostart as u8,
        cfg.tray as u8,
        cfg.threshold,
        cfg.notify as u8,
        match cfg.theme {
            ThemePref::Auto => "跟随系统",
            ThemePref::Dark => "深色",
            ThemePref::Light => "浅色",
        }
    )
}
