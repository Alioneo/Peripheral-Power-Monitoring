//! 托盘图标与气泡通知（Shell_NotifyIcon）。

use crate::sys::{self, HWND};
use std::sync::atomic::{AtomicBool, Ordering};

pub const TRAY_UID: u32 = 1;

static TRAY_SHOWN: AtomicBool = AtomicBool::new(false);

fn build(hwnd: HWND, icon: sys::HICON) -> sys::NotifyIconDataW {
    let mut d = sys::NotifyIconDataW::new(hwnd, TRAY_UID, crate::entry::WM_APP_TRAY);
    d.h_icon = icon;
    let tip = sys::wide("雷柏键鼠电量");
    let n = tip.len().min(d.tip.len());
    d.tip[..n].copy_from_slice(&tip[..n]);
    d
}

pub fn shown() -> bool {
    TRAY_SHOWN.load(Ordering::Relaxed)
}

pub fn add(hwnd: HWND, icon: sys::HICON) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let mut d = build(hwnd, icon);
        d.flags = sys::NIF_MESSAGE | sys::NIF_ICON | sys::NIF_TIP;
        let ok = sys::Shell_NotifyIconW(sys::NIM_ADD, &d) != 0;
        if ok {
            TRAY_SHOWN.store(true, Ordering::Relaxed);
        }
        ok
    }
}

/// 更新悬浮提示（两行文本用 `\n` 分隔）。值与上次相同时由调用方跳过，避免刷 NIM_MODIFY。
pub fn set_tip(hwnd: HWND, text: &str) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let mut d = build(hwnd, 0);
        d.flags = sys::NIF_TIP;
        let t = sys::wide(text);
        let n = t.len().min(d.tip.len());
        d.tip[..n].copy_from_slice(&t[..n]);
        sys::Shell_NotifyIconW(sys::NIM_MODIFY, &d) != 0
    }
}

pub fn remove(hwnd: HWND) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let mut d = build(hwnd, 0);
        d.flags = 0;
        sys::Shell_NotifyIconW(sys::NIM_DELETE, &d);
    }
    TRAY_SHOWN.store(false, Ordering::Relaxed);
}

/// 弹出一条系统通知（Win10/11 会转成通知中心条目）。调用前托盘图标必须已添加。
pub fn balloon(hwnd: HWND, title: &str, text: &str) -> bool {
    if !shown() {
        return false;
    }
    unsafe {
        let mut d = build(hwnd, 0);
        d.flags = sys::NIF_INFO;
        d.info_flags = sys::NIIF_WARNING;
        let t = sys::wide(title);
        let n = t.len().min(d.info_title.len());
        d.info_title[..n].copy_from_slice(&t[..n]);
        let m = sys::wide(text);
        let n = m.len().min(d.info.len());
        d.info[..n].copy_from_slice(&m[..n]);
        sys::Shell_NotifyIconW(sys::NIM_MODIFY, &d) != 0
    }
}
