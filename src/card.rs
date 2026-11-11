//! 取景器卡片：WebView2 宿主窗口。
//!
//! 卡片本体是设计稿原样抽取的 `assets/card.html`（CSS 数值零改动、已逐像素比对），
//! 由 WebView2 渲染，保证与设计稿 1:1。
//!
//! 生命周期遵循「点开即建、关闭即销毁」：关闭时先释放 WebView（连同其渲染进程），
//! 再销毁宿主窗口，不留后台常驻。

use std::cell::RefCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::log::Log;
use crate::sys;
use crate::sys::HWND;

const CARD_CLASS: &str = "RapooBatteryCardV1";
const CARD_HTML: &str = include_str!("../assets/card.html");

/// 卡片窗口句柄（0 = 未打开）。
static CARD: AtomicIsize = AtomicIsize::new(0);
/// 入口句柄：创建窗口时 Windows 会把 owner 提升为任务栏顶级窗口，
/// 所以重排位置时不能用 GW_OWNER，必须记住真正的入口。
static ENTRY: AtomicIsize = AtomicIsize::new(0);
/// 窗口是否已就绪（用于屏蔽创建阶段的 WM_ACTIVATE）。
static READY: AtomicBool = AtomicBool::new(false);
/// 因失焦而自动收起的时刻：入口点击据此判定「这一下是关，不是开」。
static LAST_DEACT: Mutex<Option<Instant>> = Mutex::new(None);

/// 宿主 → 卡片窗口：WebView 汇报的像素尺寸。
pub const WM_APP_CARD_SIZE: u32 = sys::WM_APP + 40;
/// 页面 → 宿主：点击卡片外留白，收起卡片。
const WM_APP_CARD_CLOSE: u32 = sys::WM_APP + 41;
/// 页面 → 宿主：设置页改动了主题 / 低电量提醒 / 阈值。
pub const WM_APP_CARD_CFG: u32 = sys::WM_APP + 42;

/// 页面提交的设置 JSON（入口线程取走即清空）。
static PENDING_CFG: Mutex<String> = Mutex::new(String::new());

/// 取出并清空页面最近一次提交的设置。
pub fn take_cfg() -> Option<String> {
    let mut g = PENDING_CFG.lock().ok()?;
    if g.is_empty() {
        None
    } else {
        Some(std::mem::take(&mut *g))
    }
}

/// 卡片窗口四周留白：卡片外投影已按用户要求在宿主内去掉，窗口即卡片本体。
const PAD: i32 = 0;
/// 卡片实测尺寸（588 定宽；高度由 CSS 内容决定，见 assets/card.html）。
const CARD_W: i32 = 588;
const CARD_H: i32 = 606;
/// 整卡等比例缩放（按设计稿红线框：829×848 → 1.413，宽高同比，不失真）。
const SCALE: f64 = 1.413;

/// CSS 像素 → 窗口物理像素（WebView2 同时按 SCALE 设置 zoom）。
fn px(v: i32) -> i32 {
    (v as f64 * SCALE).round() as i32
}
/// 卡片底边与入口顶边的间距。
const GAP: i32 = 8;

thread_local! {
    static WEB: RefCell<Option<wry::WebView>> = const { RefCell::new(None) };
}

pub fn is_open() -> bool {
    let h = CARD.load(Ordering::Relaxed);
    unsafe { h != 0 && sys::IsWindow(h) != 0 }
}

/// 入口点击：开/关卡片。`snap` 为当前实时快照 JSON（首次加载即显示真实读数）。
pub fn toggle(entry_hwnd: HWND, log: &Log, snap: &str) {
    // 点入口会让卡片窗口失焦，从而被 WM_ACTIVATE(WA_INACTIVE) 自动收起；
    // 这一次点击的语义是「关闭」，不应紧接着又打开，故短期内消费掉。
    if let Ok(mut slot) = LAST_DEACT.lock() {
        if let Some(t) = *slot {
            if t.elapsed().as_millis() < 400 {
                *slot = None;
                log.line("卡片关闭（点击入口）");
                return;
            }
        }
        *slot = None;
    }

    if is_open() {
        log.line("卡片关闭（再次点击入口）");
        close();
        return;
    }
    match unsafe { open(entry_hwnd, false, snap) } {
        Ok(h) => log.line(&format!("卡片已打开 hwnd={h}")),
        Err(e) => log.line(&format!("卡片打开失败：{e}")),
    }
}

/// 托盘菜单「设置」：打开卡片并直接进入设置页。
pub fn show_settings(entry_hwnd: HWND, log: &Log, snap: &str) {
    if let Ok(mut slot) = LAST_DEACT.lock() {
        *slot = None;
    }
    if is_open() {
        WEB.with(|w| {
            if let Some(web) = w.borrow().as_ref() {
                let _ = web.evaluate_script("openSettings()");
            }
        });
        return;
    }
    match unsafe { open(entry_hwnd, true, snap) } {
        Ok(h) => log.line(&format!("卡片（设置页）已打开 hwnd={h}")),
        Err(e) => log.line(&format!("卡片打开失败：{e}")),
    }
}

/// 推送实时快照：页面侧 `__applySnapshot` 直接替换数字/链路/在线数并重绘。
pub fn update(snap: &str) {
    if !is_open() {
        return;
    }
    WEB.with(|w| {
        if let Some(web) = w.borrow().as_ref() {
            let _ = web.evaluate_script(&format!("__applySnapshot({snap})"));
        }
    });
}

/// 关闭卡片：释放 WebView2（连同渲染进程），再销毁窗口。
pub fn close() {
    WEB.with(|w| {
        // drop 会同步销毁 ICoreWebView2Controller，释放渲染进程内存。
        let _ = w.borrow_mut().take();
    });
    unsafe {
        let h = CARD.load(Ordering::Relaxed);
        if h != 0 && sys::IsWindow(h) != 0 {
            sys::DestroyWindow(h);
        }
    }
    CARD.store(0, Ordering::Relaxed);
    ENTRY.store(0, Ordering::Relaxed);
    READY.store(false, Ordering::Relaxed);
}

/// 入口所在的显示器工作区与位置 → 卡片窗口坐标（右对齐入口、贴其上方）。
unsafe fn place(entry_hwnd: HWND, w: i32, h: i32) -> (i32, i32) {
    let mut r = sys::Rect::default();
    if sys::GetWindowRect(entry_hwnd, &mut r) == 0 {
        return (0, 0);
    }
    let card_w = w - PAD * 2;
    let card_h = h - PAD * 2;
    let mut x = r.right - card_w;
    let mut y = r.top - GAP - card_h;

    let mon = sys::MonitorFromWindow(entry_hwnd, sys::MONITOR_DEFAULTTONEAREST);
    let mut mi = sys::MonitorInfo {
        cb_size: std::mem::size_of::<sys::MonitorInfo>() as u32,
        ..Default::default()
    };
    if sys::GetMonitorInfoW(mon, &mut mi) != 0 {
        let wa = mi.rc_work;
        x = x.clamp(wa.left + 8, (wa.right - 8 - card_w).max(wa.left + 8));
        y = y.clamp(wa.top + 8, (wa.bottom - 8 - card_h).max(wa.top + 8));
    }
    (x - PAD, y - PAD)
}

unsafe fn open(entry_hwnd: HWND, with_settings: bool, snap: &str) -> Result<HWND, String> {
    register_class();
    init_com_once();

    let w = px(CARD_W + PAD * 2);
    let h = px(CARD_H + PAD * 2);
    let (x, y) = place(entry_hwnd, w, h);

    let class = sys::wide(CARD_CLASS);
    let title = sys::wide("");
    let hinst = sys::GetModuleHandleW(ptr::null());
    let hwnd = sys::CreateWindowExW(
        sys::WS_EX_TOOLWINDOW | sys::WS_EX_TOPMOST,
        class.as_ptr(),
        title.as_ptr(),
        sys::WS_POPUP,
        x,
        y,
        w,
        h,
        entry_hwnd, // owner = 入口：不占任务栏、不进 Alt+Tab
        0,
        hinst,
        ptr::null_mut(),
    );
    if hwnd == 0 {
        let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(format!("宿主窗口创建失败 err={err}"));
    }
    CARD.store(hwnd, Ordering::Relaxed);
    ENTRY.store(entry_hwnd, Ordering::Relaxed);

    let wrap = HwndWrap(hwnd);
    let hwnd_val = hwnd;
    // 缓存放数据目录：装在 Program Files 时 exe 旁不可写，localStorage 会静默失效。
    let mut web_ctx = wry::WebContext::new(Some(crate::config::data_dir().join("webview2")));
    let mut builder = wry::WebViewBuilder::new_with_web_context(&mut web_ctx)
        .with_html(CARD_HTML)
        .with_bounds(wry::Rect {
            position: wry::dpi::LogicalPosition::new(0.0, 0.0).into(),
            size: wry::dpi::PhysicalSize::new(w as u32, h as u32).into(),
        })
        .with_transparent(true)
        .with_background_color((0, 0, 0, 0))
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            if req.body().contains("\"close\":") {
                unsafe {
                    sys::PostMessageW(hwnd_val, WM_APP_CARD_CLOSE, 0, 0);
                }
                return;
            }
            // 设置页改动：转交给宿主线程写配置（页面拿不到配置文件）。
            if req.body().contains("\"cfg\":") {
                if let Ok(mut g) = PENDING_CFG.lock() {
                    g.clear();
                    g.push_str(req.body());
                }
                unsafe {
                    sys::PostMessageW(super::entry::host_hwnd(), WM_APP_CARD_CFG, 0, 0);
                }
                return;
            }
            if let Some((cw, ch)) = parse_size(req.body()) {
                unsafe {
                    sys::PostMessageW(hwnd_val, WM_APP_CARD_SIZE, cw as usize, ch as isize);
                }
            }
        });
    if with_settings {
        builder = builder.with_initialization_script("window.__wantSettings = true;");
    }
    // 首帧就用实时读数渲染，避免先闪一下设计稿里的示例数字。
    if !snap.is_empty() {
        builder = builder.with_initialization_script(format!("window.__pendingSnap={snap};"));
    }
    let web = match builder.build_as_child(&wrap) {
        Ok(v) => v,
        Err(e) => {
            sys::DestroyWindow(hwnd);
            CARD.store(0, Ordering::Relaxed);
            return Err(format!("WebView2 创建失败：{e}"));
        }
    };
    WEB.with(|slot| *slot.borrow_mut() = Some(web));

    // 整卡等比例缩放：zoom 让页面按目标尺寸重新排版（文字/描边矢量渲染，不是位图放大）。
    WEB.with(|slot| {
        if let Some(web) = slot.borrow().as_ref() {
            let _ = web.zoom(SCALE);
        }
    });

    // 圆角矩形：WebView2 的透明背景在普通 HWND 上不会得到逐像素 alpha，
    // 直接把窗口裁剪成与 CSS 相同的圆角（16px × SCALE），圆角外就露出桌面。
    apply_round_region(hwnd, w, h);

    sys::SetWindowPos(
        hwnd,
        sys::HWND_TOPMOST,
        x,
        y,
        w,
        h,
        sys::SWP_SHOWWINDOW,
    );
    READY.store(true, Ordering::Relaxed);
    Ok(hwnd)
}

/// 从 WebView 汇报的 JSON 中取出 `{"w":676,"h":694}`（不引入 JSON 依赖）。
fn parse_size(body: &str) -> Option<(i32, i32)> {
    fn field(s: &str, key: &str) -> Option<i32> {
        let i = s.find(key)?;
        let rest = &s[i + key.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            None
        } else {
            digits.parse().ok()
        }
    }
    let w = field(body, "\"w\":")?;
    let h = field(body, "\"h\":")?;
    if w >= 100 && h >= 100 && w < 4000 && h < 4000 {
        Some((w, h))
    } else {
        None
    }
}

/// 窗口圆角：优先 Win11 DWM 圆角（系统级抗锯齿，无锯齿）；失败（如 Win10）退回区域裁剪。
/// 两种路径的可见半径都是物理 8px，与卡片 CSS 的 border-radius（8/SCALE）对齐。
unsafe fn apply_round_region(hwnd: HWND, w: i32, h: i32) {
    let pref: u32 = 2; // DWMWCP_ROUND
    if sys::DwmSetWindowAttribute(hwnd, 33, &pref as *const u32 as _, 4) == 0 {
        return;
    }
    // CreateRoundRectRgn 的后两个参数是圆角椭圆的宽和高（直径）。
    let region = sys::CreateRoundRectRgn(0, 0, w + 1, h + 1, 16, 16);
    if region != 0 {
        sys::SetWindowRgn(hwnd, region, 1);
    }
}

/// 设置页展/收会改变卡片高度：据此重排窗口，保持底边贴住入口。
unsafe fn resize(hwnd: HWND, w: i32, h: i32) {
    let entry = ENTRY.load(Ordering::Relaxed);
    if entry == 0 {
        return;
    }
    let (x, y) = place(entry, px(w), px(h));
    sys::SetWindowPos(hwnd, sys::HWND_TOPMOST, x, y, px(w), px(h), 0);
    apply_round_region(hwnd, px(w), px(h));
}

unsafe extern "system" fn card_proc(hwnd: HWND, msg: u32, wp: sys::WPARAM, lp: sys::LPARAM) -> isize {
    match msg {
        WM_APP_CARD_SIZE => {
            let w = wp as i32;
            let h = lp as i32;
            if w > 0 && h > 0 {
                resize(hwnd, w, h);
            }
            0
        }
        WM_APP_CARD_CLOSE => {
            close();
            0
        }
        sys::WM_ACTIVATE => {
            let mode = (wp & 0xFFFF) as u32;
            if mode == sys::WA_INACTIVE && READY.load(Ordering::Relaxed) {
                if let Ok(mut slot) = LAST_DEACT.lock() {
                    *slot = Some(Instant::now());
                }
                close();
            }
            0
        }
        sys::WM_ERASEBKGND => 1, // 背景由 WebView2 负责，避免闪白
        sys::WM_DESTROY => {
            CARD.store(0, Ordering::Relaxed);
            READY.store(false, Ordering::Relaxed);
            0
        }
        _ => sys::DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn register_class() {
    let name = sys::wide(CARD_CLASS);
    let wc = sys::WndClassW {
        style: 0,
        lpfn_wnd_proc: card_proc as *const () as usize as isize,
        cls_extra: 0,
        wnd_extra: 0,
        h_instance: sys::GetModuleHandleW(ptr::null()),
        h_icon: 0,
        h_cursor: sys::LoadCursorW(0, sys::IDC_ARROW as *const u16),
        hbr_background: 0,
        menu_name: ptr::null(),
        class_name: name.as_ptr(),
    };
    sys::RegisterClassW(&wc);
}

/// WebView2 需要线程已初始化 COM（STA）。
fn init_com_once() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    unsafe {
        // S_OK / S_FALSE / RPC_E_CHANGED_MODE 都可继续。
        sys::CoInitializeEx(ptr::null(), sys::COINIT_APARTMENTTHREADED);
    }
}

/// 把 HWND 包一层，满足 wry 的 `HasWindowHandle`。
struct HwndWrap(HWND);

impl wry::raw_window_handle::HasWindowHandle for HwndWrap {
    fn window_handle(
        &self,
    ) -> Result<wry::raw_window_handle::WindowHandle<'_>, wry::raw_window_handle::HandleError> {
        use wry::raw_window_handle as rwh;
        let hwnd = std::num::NonZeroIsize::new(self.0).ok_or(rwh::HandleError::Unavailable)?;
        let handle = rwh::Win32WindowHandle::new(hwnd);
        Ok(unsafe { rwh::WindowHandle::borrow_raw(rwh::RawWindowHandle::Win32(handle)) })
    }
}
