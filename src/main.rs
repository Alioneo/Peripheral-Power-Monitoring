//! 雷柏键鼠电量小组件。
//!
//! 默认启动任务栏入口；`--list` / `--watch` 保留命令行只读诊断。
//! 采集内核（protocol / win / app）与界面层（sys / paint / entry）完全分离。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod card;
mod config;
mod entry;
mod log;
mod paint;
mod protocol;
mod sys;
mod tray;
mod win;

use app::Kind;
use protocol::{parse_keyboard, parse_mouse, Charge, Reading};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn charge_label(c: Charge) -> &'static str {
    match c {
        Charge::Off => "放电",
        Charge::Charging => "充电中",
        Charge::Full => "已充满",
        Charge::Unknown => "充电位无效",
    }
}

fn describe(info: &win::HidDeviceInfo) {
    println!(
        "  {:<24} usage={:04X}/{:04X} in_len={:<2}  {}",
        info.short_name(),
        info.usage_page,
        info.usage,
        info.input_report_len,
        info.path
    );
}

#[derive(Default)]
struct Stats {
    frames: u64,
    invalid: u64,
    min: u8,
    max: u8,
    last: Option<Reading>,
    charging_frames: u64,
}

impl Stats {
    fn record(&mut self, r: Reading) {
        if self.frames == 0 {
            self.min = r.battery;
            self.max = r.battery;
        }
        self.frames += 1;
        self.min = self.min.min(r.battery);
        self.max = self.max.max(r.battery);
        if matches!(r.charge, Charge::Charging | Charge::Full) {
            self.charging_frames += 1;
        }
        self.last = Some(r);
    }

    fn summary(&self, kind: Kind) -> String {
        if self.frames == 0 {
            return format!(
                "{} 统计：未收到有效帧，无效 {} 帧",
                kind.label(),
                self.invalid
            );
        }
        let last = self
            .last
            .map(|r| format!("{}% ({})", r.battery, charge_label(r.charge)))
            .unwrap_or_else(|| "无".into());
        format!(
            "{} 统计：帧 {}，无效 {}，电量 {}..{}，充电帧 {}，末次 {}",
            kind.label(),
            self.frames,
            self.invalid,
            self.min,
            self.max,
            self.charging_frames,
            last
        )
    }
}

fn print_list(devices: &[win::HidDeviceInfo], show_all: bool) {
    println!("共有 HID 接口 {} 个。", devices.len());
    let rapoo: Vec<&win::HidDeviceInfo> = devices
        .iter()
        .filter(|d| d.vendor_id == app::RAPOO_VID)
        .collect();
    println!("其中雷柏（VID_{:04X}）{} 个：", app::RAPOO_VID, rapoo.len());
    for d in &rapoo {
        describe(d);
    }
    if show_all {
        println!("全部 HID：");
        for d in devices {
            describe(d);
        }
    }
    for kind in [Kind::Mouse, Kind::Keyboard] {
        let mut found = false;
        for &pid in kind.pids() {
            if let Some(d) = app::find_device(devices, kind, pid) {
                println!(
                    "→ {} 选中（{} 0x{:04X}）：{}",
                    kind.label(),
                    app::source_label(pid),
                    pid,
                    d.short_name()
                );
                found = true;
            }
        }
        if !found {
            println!("→ {} 未找到（未连接或未唤醒）", kind.label());
        }
    }
}

/// 对照用 CLI：阻塞读、打印每帧变化与汇总。
fn watch(devices: &[win::HidDeviceInfo], seconds: Option<u64>) -> ! {
    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let (tx, rx) = mpsc::channel::<(Kind, Vec<u8>)>();
    let mut spawned = 0;

    for kind in [Kind::Mouse, Kind::Keyboard] {
        let picked = kind
            .pids()
            .iter()
            .find_map(|&pid| app::find_device(devices, kind, pid));
        if let Some(info) = picked {
            println!(
                "开始监听 {}：{}（报告长度 {}）",
                kind.label(),
                info.short_name(),
                info.input_report_len
            );
            let tx = tx.clone();
            std::thread::spawn(move || {
                let handle = match win::open(&info.path) {
                    Ok(h) => h,
                    Err(e) => {
                        println!("{} 打开失败：{e}", kind.label());
                        return;
                    }
                };
                let len = if info.input_report_len > 0 {
                    info.input_report_len
                } else {
                    64
                };
                let mut buf = vec![0u8; len.max(64)];
                loop {
                    match handle.read_raw(&mut buf) {
                        Ok(n) => {
                            if tx.send((kind, buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(500)),
                    }
                }
            });
            spawned += 1;
        }
    }
    if spawned == 0 {
        eprintln!("没有找到可监听的雷柏设备。");
        std::process::exit(1);
    }

    let mut stats: std::collections::HashMap<Kind, Stats> = Default::default();
    let mut last_battery: std::collections::HashMap<Kind, u8> = Default::default();
    let mut last_charge: std::collections::HashMap<Kind, Charge> = Default::default();
    let mut last_frame_at = Instant::now();

    loop {
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                break;
            }
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok((kind, raw)) => {
                let gap = last_frame_at.elapsed();
                last_frame_at = Instant::now();
                let parsed = match kind {
                    Kind::Mouse => parse_mouse(&raw),
                    Kind::Keyboard => parse_keyboard(&raw),
                };
                let st = stats.entry(kind).or_default();
                match parsed {
                    Some(r) => {
                        st.record(r);
                        let changed = last_battery
                            .get(&kind)
                            .map(|b| *b != r.battery)
                            .unwrap_or(true)
                            || last_charge
                                .get(&kind)
                                .map(|c| *c != r.charge)
                                .unwrap_or(true)
                            || st.frames <= 3;
                        if changed {
                            println!(
                                "[{}] {} b7={:#04X} 电量={}% {}  (+{:.1}s)",
                                sys::now_hms(),
                                kind.label(),
                                raw[7],
                                r.battery,
                                charge_label(r.charge),
                                gap.as_secs_f64()
                            );
                        }
                        last_battery.insert(kind, r.battery);
                        last_charge.insert(kind, r.charge);
                    }
                    None => {
                        st.invalid += 1;
                        println!(
                            "[{}] {} 丢弃无效帧 len={} b0={:#04X} b7={:#04X} b8={:#04X}",
                            sys::now_hms(),
                            kind.label(),
                            raw.len(),
                            raw.first().copied().unwrap_or(0),
                            raw.get(7).copied().unwrap_or(0),
                            raw.get(8).copied().unwrap_or(0)
                        );
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    println!("--- 汇总 ---");
    for kind in [Kind::Mouse, Kind::Keyboard] {
        if let Some(st) = stats.get(&kind) {
            println!("{}", st.summary(kind));
        }
    }
    std::process::exit(0);
}

fn usage() {
    println!("用法：rapoo-battery                         启动任务栏入口");
    println!("      rapoo-battery --list [--all]");
    println!("      rapoo-battery --watch [--seconds N]");
    println!("  可选：--log            写 rapoo-battery.log");
    println!("        --icon-preview <目录>   导出托盘图标 1:1 与 4 倍预览 PNG 后退出");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut list_only = false;
    let mut show_all = false;
    let mut watch_mode = false;
    let mut seconds: Option<u64> = None;
    let mut log = false;
    let mut preview_dir: Option<String> = None;
    let mut icon_from_image: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list_only = true,
            "--all" => show_all = true,
            "--watch" => watch_mode = true,
            "--log" => log = true,
            "--seconds" => {
                i += 1;
                seconds = args.get(i).and_then(|s| s.parse().ok());
            }
            "--icon-preview" => {
                i += 1;
                match args.get(i) {
                    Some(dir) => preview_dir = Some(dir.clone()),
                    None => {
                        eprintln!("--icon-preview 需要一个输出目录");
                        std::process::exit(2);
                    }
                }
            }
            "--icon-from-image" => {
                i += 1;
                match args.get(i) {
                    Some(path) => icon_from_image = Some(path.clone()),
                    None => {
                        eprintln!("--icon-from-image 需要一个图片路径");
                        std::process::exit(2);
                    }
                }
            }
            "-h" | "--help" => {
                usage();
                return;
            }
            other => {
                eprintln!("未知参数：{other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // --icon-from-image 优先于 --icon-preview（前者是位图快速预览，后者是矢量绘制）
    if let Some(img) = icon_from_image {
        let dir = preview_dir.clone().unwrap_or_else(|| ".".to_string());
        match paint::export_tray_preview_from_image(&dir, &img) {
            Ok(paths) => {
                for p in paths {
                    println!("已导出（位图预览）：{p}");
                }
            }
            Err(e) => {
                eprintln!("从图片导出托盘图标失败：{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if let Some(dir) = preview_dir {
        match paint::export_tray_preview(&dir) {
            Ok(paths) => {
                for p in paths {
                    println!("已导出（矢量绘制）：{p}");
                }
            }
            Err(e) => {
                eprintln!("导出托盘图标预览失败：{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if list_only || watch_mode {
        // 诊断模式：所有 HID 接口都读 caps，方便对照 --all 输出。
        let devices = match win::enumerate_hid_devices_all() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("枚举 HID 设备失败：{e}");
                std::process::exit(1);
            }
        };
        if list_only {
            print_list(&devices, show_all);
            return;
        }
        watch(&devices, seconds);
    }

    entry::run(entry::Options { log });
}
