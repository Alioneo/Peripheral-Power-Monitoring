//! 任务栏入口：以 Shell_TrayWnd 为父窗口的跨进程子窗口。
//!
//! 入口本身是 `WS_CHILD | WS_CLIPSIBLINGS` + `WS_EX_LAYERED | WS_EX_NOPARENTNOTIFY`，
//! 用 `UpdateLayeredWindow` 自绘；位置随任务栏重排 / DPI / 自动隐藏跟随，
//! Explorer 重启后由 500ms 巡检重建（每秒最多尝试一次、有上限、可写日志）。
//!
//! 注意：`WS_EX_LAYERED` 子窗口要求进程内嵌声明 Windows 8+ 支持的 manifest
//! （见 `app.manifest` / `build.rs`），否则 `CreateWindowExW` 直接失败。

use crate::app::{self, AppState, Kind, Link};
use crate::card;
use crate::config::{self, Config};
use crate::log::Log;
use crate::paint::{
    self, Cell, CellStatus, EntryAnim, LinkIcon, Metrics, Snap,
    Theme, ThemePref,
};
use crate::protocol::Charge;
use crate::sys::{self, CreateStructW, Msg, Rect, HWND};
use crate::tray;
use crate::win;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TRAY_CLASS: &str = "Shell_TrayWnd";
const ENTRY_CLASS: &str = "RapooBatteryEntryV1";
const HOST_CLASS: &str = "RapooBatteryHostV1";
const MUTEX_NAME: &str = "Local\\RapooBatteryEntryV1";

const WM_APP_REPOSITION: u32 = sys::WM_APP + 1;
const WM_APP_REBUILD: u32 = sys::WM_APP + 2;
const WM_APP_CLICK: u32 = sys::WM_APP + 3;
pub const WM_APP_RECONNECT: u32 = sys::WM_APP + 4;
/// 设置页改动已保存（宿主据此立即重排 / 同步托盘）。
pub const WM_APP_CONFIG_CHANGED: u32 = sys::WM_APP + 5;
/// 托盘图标回调消息。
pub const WM_APP_TRAY: u32 = sys::WM_APP + 6;

const TIMER_TICK: usize = 1;
const TIMER_TEMP_TRAY: usize = 3;
/// 动效帧定时器：只在过渡播放期间运行（33ms ≈ 30fps）。
const TIMER_ANIM: usize = 5;
const FRAME_MS: u32 = 33;
/// 托盘右键菜单项 id。
const MENU_SETTINGS: usize = 1;
const MENU_EXIT: usize = 2;

static ENTRY: AtomicIsize = AtomicIsize::new(0);
static TRAY: AtomicIsize = AtomicIsize::new(0);
static HOST: AtomicIsize = AtomicIsize::new(0);
/// 点击入口前的前台窗口（用于点击后还原焦点）。
static PREV_FG: AtomicIsize = AtomicIsize::new(0);
/// "TaskbarCreated" 广播消息 id（explorer 重启后恢复托盘图标）。
static TASKBAR_CREATED: AtomicUsize = AtomicUsize::new(0);

pub struct Options {
    pub log: bool,
}

/// 一个采集线程及其对应的设备路径（路径变化时重建）。
struct ReaderEntry {
    kind: Kind,
    /// 这条线程读的是哪一路（2.4G / 有线 PID）。
    pid: u16,
    /// 只作为 Drop 守卫：销毁即置位停止标志。
    #[allow(dead_code)]
    reader: app::Reader,
    path: String,
    /// 已请求停止、等线程真正退出后再移出列表（避免线程/句柄越积越多）。
    retiring: bool,
}

/// 全量枚举的最小间隔：只由定时器推进，不受任务栏 WinEvent 频率影响。
const ENUM_EVERY: Duration = Duration::from_millis(2500);
/// 采集重建退避：1s→2s→4s→…，上限 30s；打开成功后复位。
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

struct RetryState {
    fails: u32,
    next_at: Instant,
}

struct Ctx {
    state: Arc<Mutex<AppState>>,
    log: Log,
    config: Arc<Mutex<Config>>,
    cfg: Config,
    threshold: u8,
    /// 采集线程列表：鼠标可能有 2.4G / 有线两路，键盘本轮只有一路。
    readers: Vec<ReaderEntry>,
    /// 每个 (kind, pid) 的重建退避状态：永久打不开的设备不会每轮重试。
    reader_retry: HashMap<(Kind, u16), RetryState>,
    /// 上一次全量枚举时间（≥`ENUM_EVERY` 才再枚举）。
    last_enum: Option<Instant>,
    /// 上一次枚举是否失败（只在状态翻转时写日志）。
    enum_failed: bool,
    last_geo: Option<(i32, i32, i32, i32, u32)>,
    last_paint: Option<(i32, i32, f32)>,
    last_snap: Option<Snap>,
    /// 上次落盘的读数（`[键盘, 鼠标]`），只在拿到新读数时落盘，断连时保留旧值。
    saved: [Option<u8>; 2],
    /// 整点采样（最多 12 组，旧→新）与「上一个已见过的整点」，跨整点时才记一次。
    hist: Vec<HistPoint>,
    hour_mark: Option<u16>,
    /// 每个设备的充放电时间线（键盘、鼠标）。
    chg_log: [ChargeLog; 2],
    last_link: [Option<Link>; 2],
    last_charge: [Charge; 2],
    tick: u64,
    retry: u32,
    tray_added: bool,
    /// 托盘图标添加失败是否已写日志（避免每 500ms 重复刷屏）。
    tray_fail_logged: bool,
    /// 为发通知临时挂上的托盘图标（10 秒后摘掉）。
    temp_tray: bool,
    /// 固定托盘图标的句柄与像素尺寸；开关托盘只复用，不重建。
    tray_icon: sys::HICON,
    tray_icon_px: i32,
    notify_armed: [bool; 2],
    /// 入口绘制表面（复用 DIB，避免每帧 CreateDIBSection）。
    entry_surface: paint::Surface,
    /// 当前解析后的主题与策略。
    theme: Theme,
    theme_pref: ThemePref,
    /// 充电雷电呼吸状态。
    anim: EntryAnim,
    /// 充电雷电呼吸的起始时刻（None 表示当前无充电设备）。
    pulse_start: Option<Instant>,
    anim_timer: bool,
    /// 上次每个设备的链路图标（变化时记日志）。
    last_link_icon: [Option<LinkIcon>; 2],
    /// 上次写入托盘的提示文本（避免重复 NIM_MODIFY）。
    last_tip: String,
    /// 上次推给卡片的快照 JSON（变化才推）。
    card_json: String,
}

struct Geo {
    sx: i32,
    sy: i32,
    w: i32,
    h: i32,
    dpi: u32,
    scale: f32,
    m: Metrics,
}

/// 读取当前状态快照，同时返回每台设备的连接状态（用于日志与重绘判定）。
fn snapshot(state: &Arc<Mutex<AppState>>) -> (Snap, [Link; 2]) {
    let st = match state.lock() {
        Ok(s) => s,
        Err(_) => return (Snap::default(), [Link::Dormant; 2]),
    };
    let links = [st.link(Kind::Mouse), st.link(Kind::Keyboard)];
    let snap = Snap {
        mouse: cell(Kind::Mouse, &st.mouse, links[0]),
        keyboard: cell(Kind::Keyboard, &st.keyboard, links[1]),
    };
    (snap, links)
}

/// 上次读数落盘文件：数据目录下 `rapoo-battery.state`（内容形如 `kb=57` / `ms=86`）。
fn saved_path() -> std::path::PathBuf {
    crate::config::data_dir().join("rapoo-battery.state")
}

/// 读取上次落盘的电量（`[键盘, 鼠标]`），没有或损坏都当作 None。
fn load_saved() -> [Option<u8>; 2] {
    let mut out = [None, None];
    let Ok(txt) = std::fs::read_to_string(saved_path()) else {
        return out;
    };
    for line in txt.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let Ok(n) = v.trim().parse::<u8>() else {
            continue;
        };
        if n > 100 {
            continue;
        }
        match k.trim() {
            "kb" => out[0] = Some(n),
            "ms" => out[1] = Some(n),
            _ => {}
        }
    }
    out
}

fn save_saved(v: [Option<u8>; 2]) {
    let mut txt = String::new();
    if let Some(kb) = v[0] {
        txt.push_str(&format!("kb={kb}\n"));
    }
    if let Some(ms) = v[1] {
        txt.push_str(&format!("ms={ms}\n"));
    }
    let _ = crate::config::write_atomic(&saved_path(), &txt);
}

/// 整点采样点：12 组，最左最旧、最右最新。
#[derive(Clone, Copy)]
struct HistPoint {
    hour: u8,
    kb: Option<u8>,
    ms: Option<u8>,
}

const HIST_LEN: usize = 12;

/// 整点电量落盘文件：数据目录 `rapoo-battery.hist`，每行 `23,92,78`（缺失写 `-`）。
fn hist_path() -> std::path::PathBuf {
    saved_path().with_file_name("rapoo-battery.hist")
}

fn load_hist() -> Vec<HistPoint> {
    let mut out: Vec<HistPoint> = Vec::new();
    let Ok(txt) = std::fs::read_to_string(hist_path()) else {
        return out;
    };
    for line in txt.lines() {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() != 3 {
            continue;
        }
        let Ok(hour) = f[0].trim().parse::<u8>() else {
            continue;
        };
        if hour > 23 {
            continue;
        }
        let kb = f[1].trim().parse::<u8>().ok().filter(|v| *v <= 100);
        let ms = f[2].trim().parse::<u8>().ok().filter(|v| *v <= 100);
        out.push(HistPoint { hour, kb, ms });
    }
    if out.len() > HIST_LEN {
        out.drain(..out.len() - HIST_LEN);
    }
    out
}

/// 当前本地小时（0..23），整点采样用。
fn local_hour() -> u16 {
    let mut st = sys::SystemTime::default();
    unsafe { sys::GetLocalTime(&mut st) };
    st.hour
}

fn save_hist(h: &[HistPoint]) {
    let mut txt = String::new();
    for p in h {
        let cell = |v: Option<u8>| v.map(|x| x.to_string()).unwrap_or_else(|| "-".to_string());
        txt.push_str(&format!("{},{},{}\n", p.hour, cell(p.kb), cell(p.ms)));
    }
    let _ = crate::config::write_atomic(&hist_path(), &txt);
}

/// 历史柱状图 JSON：从旧到新，最多 12 组。
fn hist_json(h: &[HistPoint]) -> String {
    let items: Vec<String> = h
        .iter()
        .map(|p| {
            let cell = |v: Option<u8>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".to_string());
            format!(
                "{{\"h\":{},\"kb\":{},\"ms\":{}}}",
                p.hour,
                cell(p.kb),
                cell(p.ms)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// 按整点历史估算放电速率（%/h）：只统计相邻整点间「下降」的样本，充电/持平时段不计。
/// 相邻样本实际相隔几小时（程序没跑/设备离线）时按小时差折算；缺口超过 8h 视为不可信，跳过。
/// 样本不足返回 None，界面回退到标称续航折算。
fn drain_per_hour(h: &[HistPoint], pick: fn(&HistPoint) -> Option<u8>) -> Option<f32> {
    let (mut drop, mut hrs) = (0u32, 0u32);
    for w in h.windows(2) {
        if let (Some(a), Some(b)) = (pick(&w[0]), pick(&w[1])) {
            let gap = (w[1].hour as i32 - w[0].hour as i32 + 24) % 24;
            if b < a && (1..=8).contains(&gap) {
                drop += (a - b) as u32;
                hrs += gap as u32;
            }
        }
    }
    if hrs == 0 { None } else { Some(drop as f32 / hrs as f32) }
}

/// 续航估算（分钟）= 当前电量 ÷ 放电速率。速率下限 0.5%/h 防离谱，上限 99h 防刷屏。
fn rem_minutes(batt: Option<u8>, rate: Option<f32>) -> Option<u32> {
    let (b, r) = (batt?, rate?);
    Some(((b as f32 / r.max(0.5) * 60.0).round() as u32).min(99 * 60))
}

/// 卡片页需要的实时快照（JSON）：入口与大卡片共用同一份读数。
fn card_json(
    state: &Arc<Mutex<AppState>>,
    hist: &[HistPoint],
    logs: &[ChargeLog; 2],
    theme: Theme,
    cfg: Config,
) -> String {
    let st = match state.lock() {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let now = Stamp::now();
    // 放电速率：logs[0]=鼠标、logs[1]=键盘，与下面两行保持一致。
    let rate_ms = drain_per_hour(hist, |p| p.ms);
    let rate_kb = drain_per_hour(hist, |p| p.kb);
    let kb = card_dev(
        Kind::Keyboard,
        &st.keyboard,
        st.link(Kind::Keyboard),
        &logs[1],
        now,
        rem_minutes(st.keyboard.battery, rate_kb),
    );
    let ms = card_dev(
        Kind::Mouse,
        &st.mouse,
        st.link(Kind::Mouse),
        &logs[0],
        now,
        rem_minutes(st.mouse.battery, rate_ms),
    );
    let theme = if theme == Theme::Light { "light" } else { "dark" };
    // 设备名已在入口（clamp_name）剔除了引号/反斜杠/控制字符，可安全嵌入 JSON。
    format!(
        "{{\"kb\":{kb},\"ms\":{ms},\"hist\":{},\
         \"cfg\":{{\"theme\":\"{theme}\",\"notify\":{},\"threshold\":{},\
         \"autostart\":{},\"tray\":{},\"nameK\":\"{}\",\"nameM\":\"{}\"}}}}",
        hist_json(hist),
        cfg.notify,
        cfg.threshold,
        cfg.autostart,
        cfg.tray,
        cfg.kb_name,
        cfg.ms_name
    )
}

/// 本地时间戳（到分钟），只用于充电起止与使用时长的展示。
#[derive(Clone, Copy, Default)]
struct Stamp {
    year: u16,
    mon: u16,
    day: u16,
    hour: u16,
    min: u16,
}

impl Stamp {
    fn now() -> Self {
        let mut t = sys::SystemTime::default();
        unsafe { sys::GetLocalTime(&mut t) };
        Stamp {
            year: t.year,
            mon: t.month,
            day: t.day,
            hour: t.hour,
            min: t.minute,
        }
    }

    /// 例：`11月10日 23:53`
    fn label(&self) -> String {
        format!("{}月{}日 {:02}:{:02}", self.mon, self.day, self.hour, self.min)
    }

    /// 自 1970-01-01 起的天数（Howard Hinnant 的 days_from_civil）。
    fn day_index(&self) -> i64 {
        let (y, m, d) = (self.year as i64, self.mon as i64, self.day as i64);
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }

    /// 从 `self` 到 `now` 的分钟数（跨天按真实天数算）。
    fn mins_since(&self, now: &Stamp) -> i64 {
        let mins = |s: &Stamp| s.hour as i64 * 60 + s.min as i64;
        (now.day_index() - self.day_index()) * 1440 + mins(now) - mins(self)
    }

    /// 落盘格式 `2026-11-10 23:53`。
    fn encode(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.mon, self.day, self.hour, self.min
        )
    }

    fn parse(s: &str) -> Option<Self> {
        let (date, time) = s.trim().split_once(' ')?;
        let d: Vec<&str> = date.split('-').collect();
        let t: Vec<&str> = time.split(':').collect();
        if d.len() != 3 || t.len() != 2 {
            return None;
        }
        let n = |v: &str| v.parse::<u16>().ok();
        Some(Stamp {
            year: n(d[0])?,
            mon: n(d[1])?,
            day: n(d[2])?,
            hour: n(t[0])?,
            min: n(t[1])?,
        })
    }
}

/// 单个设备的充放电时间线：插上电源记 start，拔掉记 end + 当时电量。
#[derive(Clone, Copy, Default)]
struct ChargeLog {
    start: Option<Stamp>,
    end: Option<Stamp>,
    power: Option<u8>,
}

/// 时间线落盘文件：数据目录 `rapoo-battery.charge`，每行 `kb.start=2026-11-10 23:53`。
fn charge_path() -> std::path::PathBuf {
    saved_path().with_file_name("rapoo-battery.charge")
}

/// 读取落盘的时间线（`[鼠标, 键盘]`，与 `Ctx::chg_log` 同序），没有或损坏都当作空。
fn load_charge() -> [ChargeLog; 2] {
    let mut out = [ChargeLog::default(); 2];
    let Ok(txt) = std::fs::read_to_string(charge_path()) else {
        return out;
    };
    for line in txt.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        let i = if k.starts_with("ms.") {
            0
        } else if k.starts_with("kb.") {
            1
        } else {
            continue;
        };
        if v.is_empty() {
            continue;
        }
        match &k[3..] {
            "start" => out[i].start = Stamp::parse(v),
            "end" => out[i].end = Stamp::parse(v),
            "pow" => out[i].power = v.parse::<u8>().ok().filter(|p| *p <= 100),
            _ => {}
        }
    }
    out
}

fn save_charge(logs: &[ChargeLog; 2]) {
    let mut txt = String::new();
    for (i, tag) in ["ms", "kb"].iter().enumerate() {
        if let Some(s) = logs[i].start {
            txt.push_str(&format!("{tag}.start={}\n", s.encode()));
        }
        if let Some(e) = logs[i].end {
            txt.push_str(&format!("{tag}.end={}\n", e.encode()));
        }
        if let Some(p) = logs[i].power {
            txt.push_str(&format!("{tag}.pow={p}\n"));
        }
    }
    let _ = crate::config::write_atomic(&charge_path(), &txt);
}

/// 单设备快照：`v` 为电量（未连接/无读数时 null），`link` 0=2.4G、2=USB-C；`rem` 续航估算（分钟）。
fn card_dev(
    kind: Kind,
    v: &app::DeviceView,
    link: Link,
    log: &ChargeLog,
    now: Stamp,
    rem: Option<u32>,
) -> String {
    let absent = link == Link::Absent;
    let battery = match (absent, v.battery) {
        (false, Some(b)) => b.to_string(),
        _ => "null".to_string(),
    };
    let charging = v.charge == Charge::Charging;
    let charge = if charging { "true" } else { "false" };
    let present = if absent { "false" } else { "true" };
    let li = if !absent && v.wired(kind) { 2 } else { 0 };
    let q = |s: Option<String>| s.map(|v| format!("\"{v}\"")).unwrap_or_else(|| "null".to_string());
    let start = q(log.start.map(|s| s.label()));
    let end = q(log.end.map(|s| s.label()));
    let power = log.power.map(|p| p.to_string()).unwrap_or_else(|| "null".to_string());
    // 使用时长 = 当前时间 - 结束充电时间；插电中（含已充满）或还没结束过就是 null。
    let plugged = v.charge.plugged();
    let use_min = log
        .end
        .filter(|_| !plugged)
        .map(|e| e.mins_since(&now).max(0))
        .map(|m| m.to_string())
        .unwrap_or_else(|| "null".to_string());
    let rem = rem.map(|m| m.to_string()).unwrap_or_else(|| "null".to_string());
    format!(
        "{{\"v\":{battery},\"charge\":{charge},\"present\":{present},\"link\":{li},\
         \"chgStart\":{start},\"chgEnd\":{end},\"chgPow\":{power},\"useMin\":{use_min},\"remMin\":{rem}}}"
    )
}

fn cell(kind: Kind, v: &app::DeviceView, link: Link) -> Cell {
    Cell {
        battery: v.battery,
        charge: v.charge,
        status: match link {
            Link::Live => CellStatus::Live,
            Link::Dormant => CellStatus::Dormant,
            Link::Absent => CellStatus::Absent,
        },
        wired: v.wired(kind),
    }
}

fn geo(tray: HWND) -> Option<Geo> {
    unsafe {
        let mut r = Rect::default();
        if sys::GetWindowRect(tray, &mut r) == 0 {
            return None;
        }
        let tw = r.right - r.left;
        let th = r.bottom - r.top;
        if tw <= 0 || th <= 0 {
            return None;
        }
        let mut dpi = sys::GetDpiForWindow(tray);
        if dpi == 0 {
            dpi = 96;
        }
        let scale = dpi as f32 / 96.0;
        let m = paint::metrics(scale, tw, th);
        let margin = (4.0 * scale).round() as i32;
        let horizontal = tw >= th;
        let (sx, sy) = if horizontal {
            // 任务栏矩形含顶部 1~2px 边，纯居中会看着偏上，这里按 DPI 补一点下移；
            // 左边距取与上边距相同的值，四边留白一致。
            let nudge = (m.font / 18.0 * 1.5).round() as i32;
            let my = (th - m.height) / 2 + nudge;
            (r.left + my, r.top + my)
        } else {
            let x = r.left + (tw - m.width) / 2;
            (x, r.top + margin)
        };
        Some(Geo {
            sx,
            sy,
            w: m.width,
            h: m.height,
            dpi,
            scale,
            m,
        })
    }
}

fn to_client(tray: HWND, gx: i32, gy: i32) -> (i32, i32) {
    unsafe {
        let mut origin = sys::Point::default();
        sys::ClientToScreen(tray, &mut origin);
        (gx - origin.x, gy - origin.y)
    }
}

unsafe fn create_entry(ctx: &mut Ctx) {
    let tray = TRAY.load(Ordering::Relaxed);
    let g = match geo(tray) {
        Some(g) => g,
        None => {
            ctx.log.line("任务栏矩形无效，跳过创建");
            return;
        }
    };
    let (rel_x, rel_y) = to_client(tray, g.sx, g.sy);
    let class = sys::wide(ENTRY_CLASS);
    let title = sys::wide("");
    let hinst = sys::GetModuleHandleW(ptr::null());
    let entry = sys::CreateWindowExW(
        sys::WS_EX_LAYERED | sys::WS_EX_NOPARENTNOTIFY,
        class.as_ptr(),
        title.as_ptr(),
        sys::WS_CHILD | sys::WS_CLIPSIBLINGS | sys::WS_VISIBLE,
        rel_x,
        rel_y,
        g.w,
        g.h,
        tray,
        0,
        hinst,
        ptr::null_mut(),
    );
    if entry == 0 {
        ctx.log.line(&format!(
            "创建入口窗口失败，错误 {:?}",
            std::io::Error::last_os_error()
        ));
        return;
    }
    ENTRY.store(entry, Ordering::Relaxed);

    // 压制任务栏里的 XAML 兄弟窗口（Z 序顶层）。
    sys::SetWindowPos(
        entry,
        sys::HWND_TOP,
        rel_x,
        rel_y,
        g.w,
        g.h,
        sys::SWP_NOACTIVATE | sys::SWP_SHOWWINDOW,
    );

    let (snap, links) = snapshot(&ctx.state);
    ctx.theme = ctx.theme_pref.resolve(paint::system_theme());
    let ok = paint::paint_entry(
        &mut ctx.entry_surface,
        entry,
        &g.m,
        &snap,
        ctx.threshold,
        ctx.theme.pal(),
        &ctx.anim,
    );
    ctx.last_geo = Some((g.sx, g.sy, g.w, g.h, g.dpi));
    ctx.last_paint = Some((g.w, g.h, g.scale));
    ctx.last_snap = Some(snap);
    ctx.last_link = [Some(links[0]), Some(links[1])];
    // 充电记账靠「边沿」触发，初值必须是 Unknown：
    // 若直接用当前态，启动时设备已在充电则新旧相等、起始永远不记，拔电时会写出无起始的残缺记录。
    ctx.last_charge = [Charge::Unknown; 2];
    ctx.last_link_icon = [
        Some(paint::link_icon(snap.mouse)),
        Some(paint::link_icon(snap.keyboard)),
    ];
    ctx.log.line(&format!(
        "入口已创建 hwnd={} tray={} 位置=({}, {}) 尺寸={}x{} dpi={} {}",
        entry,
        tray,
        g.sx,
        g.sy,
        g.w,
        g.h,
        g.dpi,
        if ok { "绘制成功" } else { "绘制失败" }
    ));
}

/// 鼠标 2.4G / 有线 + 键盘 2.4G / 有线 四类采集目标。
fn probes() -> [(Kind, u16); 4] {
    [
        (Kind::Mouse, app::MOUSE_PID),
        (Kind::Mouse, app::MOUSE_PID_WIRED),
        (Kind::Keyboard, app::KEYBOARD_PID),
        (Kind::Keyboard, app::KEYBOARD_PID_WIRED),
    ]
}

fn kind_index(kind: Kind) -> usize {
    match kind {
        Kind::Mouse => 0,
        Kind::Keyboard => 1,
    }
}

fn reader_name(kind: Kind, pid: u16) -> String {
    if kind.wired_pid() == Some(pid) {
        format!("{}（有线 0x{:04X}）", kind.label(), pid)
    } else {
        format!(
            "{}（{} 0x{:04X}）",
            kind.label(),
            app::source_label(pid),
            pid
        )
    }
}

/// 周期性枚举：维持每台设备的在场状态，并按需启停/重建只读采集线程。
///
/// 鼠标会同时匹配 2.4G 与有线两个 PID，两路各自一条采集线程；在场状态按 Kind 聚合
/// （任一路在枚举里或任一线程仍存活即视为在场），数值只由“最近收到帧”的一路决定。
///
/// 线程退役不立刻从列表里删：先置 `retiring` 并打断阻塞读，等 `running()==false`
/// 才回收，避免反复插拔时线程/句柄叠加。永久打不开的设备按 (kind, pid) 指数退避。
fn refresh_devices(ctx: &mut Ctx) {
    // 1) 回收已经彻底退出的线程（上一轮请求停止、这一轮确认已退出）。
    ctx.readers.retain(|r| !(r.retiring && !r.reader.running()));

    let (devices, enum_ok) = match win::enumerate_hid_devices() {
        Ok(d) => (d, true),
        Err(e) => {
            // 枚举失败时不清空任何状态：按线程存活与上次在场继续保守判断。
            if !ctx.enum_failed {
                ctx.enum_failed = true;
                ctx.log
                    .line(&format!("枚举 HID 失败：{e}（沿用上次在场状态，不清空）"));
            }
            (Vec::new(), false)
        }
    };
    if enum_ok && ctx.enum_failed {
        ctx.enum_failed = false;
        ctx.log.line("HID 枚举已恢复");
    }

    let mut enum_hit = [false, false];
    for (kind, pid) in probes() {
        let ki = kind_index(kind);
        let found = if enum_ok {
            app::find_device(&devices, kind, pid)
        } else {
            None
        };
        if found.is_some() {
            enum_hit[ki] = true;
        }

        let active = ctx
            .readers
            .iter()
            .position(|r| r.kind == kind && r.pid == pid && !r.retiring);
        let retiring_pending = ctx
            .readers
            .iter()
            .any(|r| r.kind == kind && r.pid == pid && r.retiring);
        let existing =
            active.map(|i| (ctx.readers[i].path.clone(), ctx.readers[i].reader.running()));

        match (found, existing) {
            (Some(info), None) => {
                // 上一代线程仍在退出中：等它真正结束再开新句柄，线程/句柄不叠加。
                if retiring_pending {
                    continue;
                }
                if !retry_ready(ctx, kind, pid) {
                    continue;
                }
                ctx.log.line(&format!(
                    "启动 {} 采集：{}",
                    reader_name(kind, pid),
                    info.short_name()
                ));
                let reader =
                    app::spawn_reader(kind, info.clone(), pid, ctx.state.clone(), ctx.log.clone());
                ctx.readers.push(ReaderEntry {
                    kind,
                    pid,
                    reader,
                    path: info.path,
                    retiring: false,
                });
            }
            (Some(info), Some((path, alive))) => {
                if path != info.path {
                    // 路径变化（接口重建）：旧线程先退役，下一轮再开新句柄。
                    ctx.log.line(&format!(
                        "{} 设备路径变化，重建采集",
                        reader_name(kind, pid)
                    ));
                    retire_reader(ctx, kind, pid);
                    // 这是接口重建，不是“打不开”：不进入退避。
                    ctx.reader_retry.remove(&(kind, pid));
                } else if !alive {
                    // 线程提前退出（打开句柄失败/连续读错误）：按级数退避重建。
                    let delay = bump_retry(ctx, kind, pid);
                    ctx.log.line(&format!(
                        "{} 采集线程已退出，{} 秒后重建（第 {} 次失败）",
                        reader_name(kind, pid),
                        delay.as_secs(),
                        ctx.reader_retry
                            .get(&(kind, pid))
                            .map(|s| s.fails)
                            .unwrap_or(1)
                    ));
                    retire_reader(ctx, kind, pid);
                } else {
                    // 句柄稳定运行：复位退避，偶发一次失败不会拖慢后续重连。
                    ctx.reader_retry.remove(&(kind, pid));
                }
            }
            (None, Some((_path, true))) => {
                // 枚举暂时拿不到，但句柄还停在阻塞读上：这是休眠（设备省电时接口可能
                // 暂时不出现），保留采集线程，等它醒来后第一帧直接读到，不能按断开处理。
                ctx.reader_retry.remove(&(kind, pid));
            }
            (None, Some((_path, false))) => {
                // 枚举里没有、读取句柄也已失效：确认断开，才允许开始宽限期计时。
                ctx.log.line(&format!(
                    "{} 已离开枚举结果且读取句柄失效，停止采集线程",
                    reader_name(kind, pid)
                ));
                if let Some(i) = active {
                    ctx.readers.remove(i);
                }
                ctx.reader_retry.remove(&(kind, pid));
                // 有线口确定拔掉：清掉来源标记，图标/文案回退到 2.4G（读数保留）。
                clear_wired_source(ctx, kind, pid);
            }
            (None, None) => {
                // 设备不在场：清掉历史退避，重插后立即重连，不被旧的失败计数拖慢。
                ctx.reader_retry.remove(&(kind, pid));
            }
        }
    }

    // 在场按 Kind 聚合：鼠标两路里任一路仍在枚举/仍在读，就说明鼠标还在，不能变灰。
    // 枚举失败时沿用上次的在场值（保守处理，不清空已有状态）。
    for (i, kind) in [Kind::Mouse, Kind::Keyboard].iter().enumerate() {
        let reader_alive = ctx
            .readers
            .iter()
            .any(|r| !r.retiring && r.kind == *kind && r.reader.running());
        let in_enum = if enum_ok {
            enum_hit[i]
        } else {
            current_present(ctx, *kind)
        };
        set_present(ctx, *kind, in_enum, reader_alive);
    }
}

fn current_present(ctx: &Ctx, kind: Kind) -> bool {
    match ctx.state.lock() {
        Ok(st) => st.view(kind).present,
        Err(_) => false,
    }
}

/// 请求某一路的采集线程退役（打断阻塞读）；真正回收留到下一次巡检。
fn retire_reader(ctx: &mut Ctx, kind: Kind, pid: u16) {
    if let Some(i) = ctx
        .readers
        .iter()
        .position(|r| r.kind == kind && r.pid == pid && !r.retiring)
    {
        ctx.readers[i].retiring = true;
        ctx.readers[i].reader.stop();
    }
}

/// 有线口确实退役且不在枚举里：清掉来源标记（只清匹配的 PID，读数不动）。
fn clear_wired_source(ctx: &Ctx, kind: Kind, pid: u16) {
    if kind.wired_pid() != Some(pid) {
        return;
    }
    if let Ok(mut st) = ctx.state.lock() {
        st.clear_source(kind, pid);
    }
    ctx.log.line(&format!(
        "{} 有线供数已失效，外观回退（读数与充电态保留）",
        kind.label()
    ));
}

fn retry_ready(ctx: &Ctx, kind: Kind, pid: u16) -> bool {
    match ctx.reader_retry.get(&(kind, pid)) {
        Some(s) => Instant::now() >= s.next_at,
        None => true,
    }
}

/// 记一次失败并按指数退避安排下一次重试，返回本次延迟。
fn bump_retry(ctx: &mut Ctx, kind: Kind, pid: u16) -> Duration {
    let state = ctx.reader_retry.entry((kind, pid)).or_insert(RetryState {
        fails: 0,
        next_at: Instant::now(),
    });
    let delay = RETRY_BASE
        .saturating_mul(1u32 << state.fails.min(5))
        .min(RETRY_MAX);
    state.fails = state.fails.saturating_add(1);
    state.next_at = Instant::now() + delay;
    delay
}

fn set_present(ctx: &Ctx, kind: Kind, in_enum: bool, reader_alive: bool) {
    if let Ok(mut st) = ctx.state.lock() {
        st.set_present(kind, in_enum, reader_alive);
    }
}

fn charge_label(c: Charge) -> &'static str {
    match c {
        Charge::Off => "放电",
        Charge::Charging => "充电中",
        Charge::Full => "已充满",
        Charge::Unknown => "未知",
    }
}

/// 取固定托盘图标（按系统小图标尺寸从内嵌 ICO 选取，只创建一次，复用）。
fn ensure_tray_icon(ctx: &mut Ctx) -> sys::HICON {
    if ctx.tray_icon != 0 {
        return ctx.tray_icon;
    }
    let px = unsafe { sys::GetSystemMetrics(sys::SM_CXSMICON) }.max(16);
    match paint::tray_icon(px) {
        Some(icon) => {
            ctx.tray_icon = icon;
            ctx.tray_icon_px = px;
            icon
        }
        None => {
            ctx.log.line("托盘图标绘制失败");
            0
        }
    }
}

unsafe fn sync_tray(ctx: &mut Ctx) {
    let host = HOST.load(Ordering::Relaxed);
    if host == 0 {
        return;
    }
    if ctx.cfg.tray && !ctx.tray_added {
        let icon = ensure_tray_icon(ctx);
        if icon != 0 && tray::add(host, icon) {
            ctx.tray_added = true;
            ctx.temp_tray = false;
            ctx.tray_fail_logged = false;
            ctx.log.line("托盘图标已添加");
        } else if !ctx.tray_fail_logged {
            // Shell 繁忙时失败会每 500ms 重试，只在首次失败时写日志。
            ctx.tray_fail_logged = true;
            ctx.log.line("托盘图标添加失败（将自动重试）");
        }
    } else if !ctx.cfg.tray && ctx.tray_added && !ctx.temp_tray {
        tray::remove(host);
        ctx.tray_added = false;
        ctx.log.line("托盘图标已移除");
    }
}

/// 低电量提醒状态机的单步转移（纯函数，便于穷举边界）。
///
/// 输入当前是否已武装、设备电量、阈值、连接状态，返回 (新武装态, 是否应提醒)。
/// - 只在 `Live` 且电量 <= 阈值时提醒，且同一阶段只提醒一次（武装后不再重复）；
/// - 电量回到 `阈值 + 5%` 以上才重新武装，避免在阈值附近抖动反复轰炸；
/// - 设备判定为 `Absent` 或数据缺失时解除武装，重新出现后按新阶段再提醒。
pub fn low_battery_step(
    armed: bool,
    battery: Option<u8>,
    threshold: u8,
    link: Link,
) -> (bool, bool) {
    match (battery, link) {
        (Some(b), Link::Live) => {
            if b <= threshold {
                if armed {
                    (true, false)
                } else {
                    (true, true)
                }
            } else if b > threshold.saturating_add(5) {
                (false, false)
            } else {
                (armed, false)
            }
        }
        (_, Link::Absent) => (false, false),
        _ => (armed, false),
    }
}

/// 低电量提醒：同一设备同一低电阶段只提醒一次，回到阈值 +5% 以上重新武装。
fn check_low_battery(ctx: &mut Ctx) {
    if !ctx.cfg.notify {
        ctx.notify_armed = [false, false];
        return;
    }
    let (links, batteries) = match ctx.state.lock() {
        Ok(st) => (
            [st.link(Kind::Mouse), st.link(Kind::Keyboard)],
            [st.mouse.battery, st.keyboard.battery],
        ),
        Err(_) => return,
    };
    let kinds = [Kind::Mouse, Kind::Keyboard];
    for i in 0..2 {
        let (armed, fire) = low_battery_step(
            ctx.notify_armed[i],
            batteries[i],
            ctx.cfg.threshold,
            links[i],
        );
        ctx.notify_armed[i] = armed;
        if fire {
            let b = batteries[i].unwrap_or(0);
            let text = format!(
                "{}电量 {}%（阈值 {}%）",
                kinds[i].label(),
                b,
                ctx.cfg.threshold
            );
            ctx.log.line(&format!(
                "低电量提醒：{text}（本阶段仅此一次，回充到阈值+5% 以上才重新武装）"
            ));
            unsafe { send_notification(ctx, &text) };
        }
    }
}

unsafe fn send_notification(ctx: &mut Ctx, text: &str) {
    let host = HOST.load(Ordering::Relaxed);
    if host == 0 {
        return;
    }
    if !tray::shown() {
        let icon = ensure_tray_icon(ctx);
        if icon == 0 || !tray::add(host, icon) {
            ctx.log.line("临时托盘图标创建失败，系统通知未发出");
            return;
        }
        ctx.tray_added = true;
        ctx.temp_tray = true;
        sys::SetTimer(host, TIMER_TEMP_TRAY, 10000, 0);
        ctx.log
            .line("临时挂载托盘图标用于系统通知（10 秒后自动移除）");
    }
    if tray::balloon(host, "雷柏键鼠低电量", text) {
        ctx.log.line("系统通知已发送");
    } else {
        ctx.log.line("系统通知发送失败");
    }
}

/// 托盘悬浮提示：仅两行（键盘/鼠标），数字实时取自当前快照。
fn battery_tip(snap: &Snap) -> String {
    fn part(label: &str, cell: Cell) -> String {
        match cell.battery {
            Some(b) if cell.status != CellStatus::Absent => format!("{label}电量{b}%"),
            _ => format!("{label}电量--"),
        }
    }
    format!(
        "{}\n{}",
        part("键盘", snap.keyboard),
        part("鼠标", snap.mouse)
    )
}

/// 值变化时才写 NIM_MODIFY，避免每 500ms 都触发一次 Shell 更新。
unsafe fn update_tooltip(ctx: &mut Ctx) {
    let host = HOST.load(Ordering::Relaxed);
    if host == 0 || !ctx.tray_added {
        return;
    }
    let (snap, _) = snapshot(&ctx.state);
    let tip = battery_tip(&snap);
    if tip != ctx.last_tip && tray::set_tip(host, &tip) {
        ctx.last_tip = tip;
    }
}

/// 从扁平 JSON 里取字段值（字符串去引号，数字/布尔按原样），项目不引 JSON 依赖。
fn json_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let rest = &json[json.find(&pat)? + pat.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    if let Some(r) = rest.strip_prefix('"') {
        r.find('"').map(|e| &r[..e])
    } else {
        let end = rest
            .find([',', '}', ' '])
            .unwrap_or(rest.len());
        Some(&rest[..end])
    }
}

/// 设置页（卡片）提交的改动：主题 / 低电量提醒 / 阈值写回配置并立即生效。
fn apply_card_cfg(ctx: &mut Ctx, json: &str) {
    let prev = match ctx.config.lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    let mut next = prev.clone();
    if let Some(v) = json_field(json, "theme") {
        next.theme = if v.eq_ignore_ascii_case("light") {
            ThemePref::Light
        } else if v.eq_ignore_ascii_case("dark") {
            ThemePref::Dark
        } else {
            ThemePref::Auto
        };
    }
    if let Some(v) = json_field(json, "notify") {
        next.notify = v == "true" || v == "1";
    }
    if let Some(v) = json_field(json, "threshold") {
        if let Ok(n) = v.parse::<u8>() {
            next.threshold = n.clamp(5, 60);
        }
    }
    if let Some(v) = json_field(json, "autostart") {
        next.autostart = v == "true" || v == "1";
    }
    if let Some(v) = json_field(json, "tray") {
        next.tray = v == "true" || v == "1";
    }
    if let Some(v) = json_field(json, "nameK") {
        next.kb_name = config::clamp_name(v);
    }
    if let Some(v) = json_field(json, "nameM") {
        next.ms_name = config::clamp_name(v);
    }
    if next == prev {
        return;
    }
    if let Ok(mut g) = ctx.config.lock() {
        *g = next.clone();
    }
    if next.autostart != prev.autostart {
        if let Err(e) = config::apply_autostart(next.autostart) {
            ctx.log.line(&format!("开机自启设置失败：{e}"));
        }
    }
    if let Err(e) = next.save() {
        ctx.log.line(&format!("设置保存失败：{e}"));
    }
    ctx.log
        .line(&format!("设置页更新：{}", config::describe(&next)));
    let host = HOST.load(Ordering::Relaxed);
    if host != 0 {
        unsafe { sys::PostMessageW(host, WM_APP_CONFIG_CHANGED, 0, 0) };
    }
}

fn resolve_theme(ctx: &mut Ctx) -> bool {
    let t = ctx.theme_pref.resolve(paint::system_theme());
    if t == ctx.theme {
        return false;
    }
    ctx.theme = t;
    true
}

/// 推进充电雷电呼吸相位，返回是否需要重绘。
fn advance_anim(ctx: &mut Ctx) -> bool {
    let start = match ctx.pulse_start {
        Some(t) => t,
        None => return false,
    };
    let secs = Instant::now().saturating_duration_since(start).as_secs_f32();
    ctx.anim.pulse = (secs / 1.9) % 1.0;
    true
}

unsafe fn tick(ctx: &mut Ctx) {
    // 设置页改动会通过互斥量 + WM_APP_CONFIG_CHANGED 立即反映到这里。
    let cfg = match ctx.config.lock() {
        Ok(c) => c.clone(),
        Err(_) => ctx.cfg.clone(),
    };
    if cfg != ctx.cfg {
        ctx.log
            .line(&format!("配置即时生效：{}", config::describe(&cfg)));
        ctx.threshold = cfg.threshold;
        ctx.theme_pref = cfg.theme;
        ctx.cfg = cfg;
        ctx.last_geo = None;
        ctx.last_snap = None;
        ctx.last_tip.clear();
    }
    // 跟随系统时每次巡检都读一次注册表（2 次/秒，开销可忽略）；
    // 明确深/浅色时不读。系统主题翻转后强制重绘一次。
    if resolve_theme(ctx) {
        ctx.last_snap = None;
        ctx.log.line("系统主题已切换，入口配色更新");
    }
    unsafe {
        sync_tray(ctx);
        check_low_battery(ctx);
        update_tooltip(ctx);
        let class = sys::wide(TRAY_CLASS);
        let tray = sys::FindWindowW(class.as_ptr(), ptr::null());

        let current = ENTRY.load(Ordering::Relaxed);
        if current != 0 && sys::IsWindow(current) == 0 {
            ENTRY.store(0, Ordering::Relaxed);
        }

        if tray == 0 {
            if ctx.tick.is_multiple_of(2) {
                ctx.retry = ctx.retry.saturating_add(1);
                if ctx.retry <= 60 {
                    if ctx.retry % 5 == 1 {
                        ctx.log
                            .line(&format!("未找到任务栏，第 {} 次重试（上限 60）", ctx.retry));
                    }
                } else if ctx.tick.is_multiple_of(20) {
                    ctx.log.line(&format!(
                        "任务栏长期缺失（已试 {} 次），降频等待 Explorer",
                        ctx.retry
                    ));
                }
            }
            return;
        }
        TRAY.store(tray, Ordering::Relaxed);

        let current = ENTRY.load(Ordering::Relaxed);
        if current != 0 && sys::GetParent(current) != tray {
            ctx.log.line("入口的父窗口已变化，销毁重建");
            sys::DestroyWindow(current);
            ENTRY.store(0, Ordering::Relaxed);
        }

        let current = ENTRY.load(Ordering::Relaxed);
        if current == 0 {
            create_entry(ctx);
        } else if let Some(g) = geo(tray) {
            // Explorer 重启后会新建内容桥并把它压在入口之上，发现即重新抬到顶层。
            if sys::GetWindow(tray, sys::GW_CHILD) != current {
                sys::SetWindowPos(
                    current,
                    sys::HWND_TOP,
                    0,
                    0,
                    0,
                    0,
                    sys::SWP_NOMOVE | sys::SWP_NOSIZE | sys::SWP_NOACTIVATE,
                );
            }
            let key = (g.sx, g.sy, g.w, g.h, g.dpi);
            let mut need_paint = false;
            if ctx.last_geo != Some(key) {
                let (rel_x, rel_y) = to_client(tray, g.sx, g.sy);
                sys::SetWindowPos(
                    current,
                    sys::HWND_TOP,
                    rel_x,
                    rel_y,
                    g.w,
                    g.h,
                    sys::SWP_NOACTIVATE,
                );
                ctx.last_geo = Some(key);
                if ctx.last_paint != Some((g.w, g.h, g.scale)) {
                    need_paint = true;
                }
            }
            let (snap, links) = snapshot(&ctx.state);
            // 拿到新读数就落盘：下次启动时键鼠若还在休眠，先用它顶上。
            let mut changed = false;
            for (i, v) in [snap.keyboard.battery, snap.mouse.battery].iter().enumerate() {
                if let Some(x) = *v {
                    if ctx.saved[i] != Some(x) {
                        ctx.saved[i] = Some(x);
                        changed = true;
                    }
                }
            }
            if changed {
                save_saved(ctx.saved);
            }
            // 整点采样：只在「运行中跨过整点」时记一组，启动当刻不补记。
            let hour = local_hour();
            match ctx.hour_mark {
                None => ctx.hour_mark = Some(hour),
                Some(prev) if prev != hour => {
                    ctx.hour_mark = Some(hour);
                    let pt = HistPoint {
                        hour: hour as u8,
                        kb: snap.keyboard.battery,
                        ms: snap.mouse.battery,
                    };
                    if ctx.hist.len() >= HIST_LEN {
                        ctx.hist.remove(0);
                    }
                    ctx.hist.push(pt);
                    save_hist(&ctx.hist);
                    ctx.log.line(&format!(
                        "整点采样 {:02}:00 → 键盘 {} / 鼠标 {}",
                        pt.hour,
                        pt.kb.map(|v| format!("{v}%")).unwrap_or_else(|| "--".to_string()),
                        pt.ms.map(|v| format!("{v}%")).unwrap_or_else(|| "--".to_string()),
                    ));
                }
                _ => {}
            }
            for (i, kind) in [Kind::Mouse, Kind::Keyboard].iter().enumerate() {
                if ctx.last_link[i] != Some(links[i]) {
                    ctx.log.line(&format!(
                        "{} {}",
                        kind.label(),
                        match links[i] {
                            Link::Live => "进入使用中",
                            Link::Dormant => "转为休眠（保留上次数值与外观）",
                            Link::Absent => "判定为未连接",
                        }
                    ));
                    ctx.last_link[i] = Some(links[i]);
                }
                let charge = if i == 0 {
                    snap.mouse.charge
                } else {
                    snap.keyboard.charge
                };
                let battery = if i == 0 {
                    snap.mouse.battery
                } else {
                    snap.keyboard.battery
                };
                // 充放电时间线：插电记起始，拔电记结束 + 当时电量。
                // 「插电」= 充电中或已充满；键盘休眠不发帧时充电态保持 Unknown，一律不动记录，
                // 等唤醒后拿到确定状态再按边沿入账：
                //   确定插电且没有未结束的起始 → 记起始（上次充着电退出/休眠的沿用落盘起始）；
                //   确定拔电且（之前在插电 或 有未结束的起始）→ 记结束 + 当前电量。
                let now = Stamp::now();
                if ctx.last_charge[i] != charge {
                    let was_plugged = ctx.last_charge[i].plugged();
                    let pending = ctx.chg_log[i].start.is_some() && ctx.chg_log[i].end.is_none();
                    if charge.plugged() {
                        // 充电中 <-> 已充满 属于同一段插电，不重开起始。
                        if !was_plugged && !pending {
                            ctx.chg_log[i] = ChargeLog {
                                start: Some(now),
                                end: None,
                                power: None,
                            };
                            ctx.log.line(&format!("{} 充电起始 {}", kind.label(), now.label()));
                            save_charge(&ctx.chg_log);
                        }
                    } else if was_plugged || pending {
                        ctx.chg_log[i].end = Some(now);
                        ctx.chg_log[i].power = battery;
                        ctx.log.line(&format!(
                            "{} 充电结束 {}（充至 {}）",
                            kind.label(),
                            now.label(),
                            battery
                                .map(|v| format!("{v}%"))
                                .unwrap_or_else(|| "--".to_string())
                        ));
                        save_charge(&ctx.chg_log);
                    }
                }
                if ctx.last_charge[i] != charge {
                    ctx.log.line(&format!(
                        "{} 充电态 -> {}",
                        kind.label(),
                        charge_label(charge)
                    ));
                    ctx.last_charge[i] = charge;
                }
            }
            // 链路图标：按真实链路（无线/有线）+ 充电雷电；变化时只记日志。
            let icons = [paint::link_icon(snap.mouse), paint::link_icon(snap.keyboard)];
            for (i, &icon) in icons.iter().enumerate() {
                if let Some(prev) = ctx.last_link_icon[i] {
                    if prev != icon {
                        ctx.log.line(&format!(
                            "{} 链路图标 {} -> {}",
                            if i == 0 { "鼠标" } else { "键盘" },
                            link_icon_label(prev),
                            link_icon_label(icon)
                        ));
                    }
                }
                ctx.last_link_icon[i] = Some(icon);
            }
            // 有设备在充电时，雷电图标按 1.9s 周期呼吸，需要持续出帧。
            let pulsing = icons.contains(&LinkIcon::Bolt);
            ctx.anim.pulse_on = pulsing;
            if pulsing {
                if ctx.pulse_start.is_none() {
                    ctx.pulse_start = Some(Instant::now());
                }
            } else {
                ctx.pulse_start = None;
                ctx.anim.pulse = 0.0;
            }
            if ctx.last_snap != Some(snap) {
                ctx.last_snap = Some(snap);
                need_paint = true;
            }
            if ctx.anim.busy() {
                need_paint = true;
                if !ctx.anim_timer {
                    sys::SetTimer(HOST.load(Ordering::Relaxed), TIMER_ANIM, FRAME_MS, 0);
                    ctx.anim_timer = true;
                }
            }
            if need_paint
                && !paint::paint_entry(
                    &mut ctx.entry_surface,
                    current,
                    &g.m,
                    &snap,
                    ctx.threshold,
                    ctx.theme.pal(),
                    &ctx.anim,
                )
            {
                ctx.log.line("UpdateLayeredWindow 绘制失败");
            }
            if need_paint {
                ctx.last_paint = Some((g.w, g.h, g.scale));
            }
            // 卡片开着时把同一份快照推给页面，数字/链路/在线数与入口保持一致。
            if card::is_open() {
                let json = card_json(&ctx.state, &ctx.hist, &ctx.chg_log, ctx.theme, ctx.cfg.clone());
                if !json.is_empty() && json != ctx.card_json {
                    card::update(&json);
                    ctx.card_json = json;
                }
            }
        }

        ctx.retry = 0;
    }
}

fn link_icon_label(i: LinkIcon) -> &'static str {
    match i {
        LinkIcon::Radio => "2.4G",
        LinkIcon::Usb => "USB-C",
        LinkIcon::Bolt => "充电",
    }
}

/// 动效帧：推进进度并重绘入口；播完立即关掉帧定时器回到 500ms 低频巡检。
unsafe fn anim_frame(ctx: &mut Ctx) {
    if !advance_anim(ctx) {
        if ctx.anim_timer {
            sys::KillTimer(HOST.load(Ordering::Relaxed), TIMER_ANIM);
            ctx.anim_timer = false;
        }
        return;
    }
    let tray = TRAY.load(Ordering::Relaxed);
    let current = ENTRY.load(Ordering::Relaxed);
    if tray != 0 && current != 0 {
        if let Some(g) = geo(tray) {
            let (snap, _) = snapshot(&ctx.state);
            paint::paint_entry(
                &mut ctx.entry_surface,
                current,
                &g.m,
                &snap,
                ctx.threshold,
                ctx.theme.pal(),
                &ctx.anim,
            );
        }
    }
    if !ctx.anim.busy() && ctx.anim_timer {
        sys::KillTimer(HOST.load(Ordering::Relaxed), TIMER_ANIM);
        ctx.anim_timer = false;
        ctx.last_snap = None;
    }
}

/// 全量枚举节流：任务栏 WinEvent 再密集也不会放大成高频 HID 枚举。
unsafe fn maybe_refresh_devices(ctx: &mut Ctx) {
    let now = Instant::now();
    if let Some(t) = ctx.last_enum {
        if now.saturating_duration_since(t) < ENUM_EVERY {
            return;
        }
    }
    ctx.last_enum = Some(now);
    refresh_devices(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_rate_only_counts_dropping_hours() {
        let h = |hour: u8, kb: Option<u8>| HistPoint { hour, kb, ms: None };
        // 下降 3%/h + 充电（上升）+ 持平：只有第一段计入。
        let hist = vec![h(9, Some(90)), h(10, Some(87)), h(11, Some(95)), h(12, Some(95))];
        assert_eq!(drain_per_hour(&hist, |p| p.kb), Some(3.0));
        assert_eq!(drain_per_hour(&[], |p| p.kb), None, "空历史应回退标称");
    }

    #[test]
    fn drain_rate_divides_by_real_hour_gap() {
        let h = |hour: u8, v: Option<u8>| HistPoint { hour, kb: v, ms: None };
        // 跨 7 小时掉 35%：应按 5%/h，而不是 35%/h。
        let hist = vec![h(16, Some(100)), h(23, Some(65))];
        assert_eq!(drain_per_hour(&hist, |p| p.kb), Some(5.0));
        // 缺口超过 8h 视为不可信 → 无样本回退标称。
        let stale = vec![h(1, Some(100)), h(20, Some(10))];
        assert_eq!(drain_per_hour(&stale, |p| p.kb), None);
    }

    #[test]
    fn rem_minutes_has_floor_and_cap() {
        assert_eq!(rem_minutes(Some(90), Some(3.0)), Some(30 * 60));
        assert_eq!(rem_minutes(Some(100), Some(0.1)), Some(99 * 60), "速率下限 + 99h 上限");
        assert_eq!(rem_minutes(None, Some(3.0)), None);
        assert_eq!(rem_minutes(Some(50), None), None);
    }

    #[test]
    fn fires_once_per_stage() {
        assert_eq!(
            low_battery_step(false, Some(20), 20, Link::Live),
            (true, true)
        );
        assert_eq!(
            low_battery_step(true, Some(18), 20, Link::Live),
            (true, false)
        );
        assert_eq!(
            low_battery_step(true, Some(20), 20, Link::Live),
            (true, false)
        );
    }

    #[test]
    fn hysteresis_needs_threshold_plus_five() {
        for b in 21..=25 {
            assert_eq!(
                low_battery_step(true, Some(b), 20, Link::Live),
                (true, false),
                "21..25 不应解除武装"
            );
        }
        assert_eq!(
            low_battery_step(true, Some(26), 20, Link::Live),
            (false, false)
        );
        assert_eq!(
            low_battery_step(false, Some(19), 20, Link::Live),
            (true, true)
        );
    }

    #[test]
    fn dormant_and_missing_do_not_fire() {
        assert_eq!(
            low_battery_step(false, Some(10), 20, Link::Dormant),
            (false, false)
        );
        assert_eq!(
            low_battery_step(true, Some(10), 20, Link::Dormant),
            (true, false)
        );
        assert_eq!(
            low_battery_step(false, None, 20, Link::Live),
            (false, false)
        );
    }

    #[test]
    fn absent_disarms() {
        assert_eq!(
            low_battery_step(true, Some(10), 20, Link::Absent),
            (false, false)
        );
    }

    #[test]
    fn threshold_edges() {
        assert_eq!(
            low_battery_step(false, Some(0), 0, Link::Live),
            (true, true)
        );
        assert_eq!(
            low_battery_step(true, Some(6), 0, Link::Live),
            (false, false)
        );
        assert_eq!(
            low_battery_step(false, Some(100), 100, Link::Live),
            (true, true)
        );
        assert_eq!(
            low_battery_step(true, Some(100), 255, Link::Live),
            (true, false)
        );
    }

    #[test]
    fn tooltip_is_two_lines_with_live_values() {
        let cell = |b, status| Cell {
            battery: b,
            charge: Charge::Off,
            status,
            wired: false,
        };
        let mut s = Snap::default();
        s.keyboard = cell(Some(65), CellStatus::Live);
        s.mouse = cell(Some(25), CellStatus::Live);
        assert_eq!(battery_tip(&s), "键盘电量65%\n鼠标电量25%");
        // 休眠保留读数，未连接显示 --。
        s.keyboard.status = CellStatus::Dormant;
        assert_eq!(battery_tip(&s), "键盘电量65%\n鼠标电量25%");
        s.mouse.status = CellStatus::Absent;
        assert_eq!(battery_tip(&s), "键盘电量65%\n鼠标电量--");
        s.keyboard.battery = None;
        assert_eq!(battery_tip(&s), "键盘电量--\n鼠标电量--");
    }
}

/// 把前台还给点击前的窗口（跨线程时先 AttachThreadInput 才能抢回）。
unsafe fn restore_foreground(target: HWND) {
    let target_thread = sys::GetWindowThreadProcessId(target, ptr::null_mut());
    let cur = sys::GetCurrentThreadId();
    if target_thread != 0 && target_thread != cur {
        sys::AttachThreadInput(cur, target_thread, 1);
        sys::SetForegroundWindow(target);
        sys::AttachThreadInput(cur, target_thread, 0);
    } else {
        sys::SetForegroundWindow(target);
    }
}

unsafe extern "system" fn entry_proc(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        sys::WM_MOUSEACTIVATE => {
            // 点入口前的前台窗口，点击后原样还给它（不抢焦点）。
            let fg = sys::GetForegroundWindow();
            if fg != 0 {
                PREV_FG.store(fg, Ordering::Relaxed);
            }
            sys::MA_NOACTIVATE
        }
        sys::WM_ERASEBKGND => 1,
        sys::WM_LBUTTONUP => {
            let host = HOST.load(Ordering::Relaxed);
            if host != 0 {
                sys::PostMessageW(host, WM_APP_CLICK, 0, 0);
            }
            let fg = PREV_FG.load(Ordering::Relaxed);
            if fg != 0 && sys::GetForegroundWindow() != fg {
                restore_foreground(fg);
            }
            0
        }
        sys::WM_DPICHANGED_AFTERPARENT => {
            let host = HOST.load(Ordering::Relaxed);
            if host != 0 {
                sys::PostMessageW(host, WM_APP_REPOSITION, 0, 0);
            }
            0
        }
        sys::WM_DESTROY => {
            let host = HOST.load(Ordering::Relaxed);
            if host != 0 {
                sys::PostMessageW(host, WM_APP_REBUILD, 0, 0);
            }
            0
        }
        sys::WM_NCDESTROY => {
            ENTRY.store(0, Ordering::Relaxed);
            sys::DefWindowProcW(hwnd, msg, w, l)
        }
        _ => sys::DefWindowProcW(hwnd, msg, w, l),
    }
}

/// 面板等子模块需要向宿主投递消息时使用。
pub fn host_hwnd() -> HWND {
    HOST.load(Ordering::Relaxed)
}

unsafe fn toggle_panel(ctx: &mut Ctx) {
    let entry = ENTRY.load(Ordering::Relaxed);
    if entry != 0 {
        // 大卡片走 WebView2（设计稿 1:1），点开即建、关闭即销毁释放。
        let snap = card_json(&ctx.state, &ctx.hist, &ctx.chg_log, ctx.theme, ctx.cfg.clone());
        card::toggle(entry, &ctx.log, &snap);
        ctx.card_json = snap;
    }
}

/// 托盘右键菜单：仅「设置」「退出」。左键不响应（用户明确要求）。
unsafe fn show_tray_menu(hwnd: HWND, ctx: &mut Ctx) {
    let menu = sys::CreatePopupMenu();
    if menu == 0 {
        return;
    }
    let settings = sys::wide("设置");
    let exit = sys::wide("退出");
    sys::AppendMenuW(menu, sys::MF_STRING, MENU_SETTINGS, settings.as_ptr());
    sys::AppendMenuW(menu, sys::MF_SEPARATOR, 0, ptr::null());
    sys::AppendMenuW(menu, sys::MF_STRING, MENU_EXIT, exit.as_ptr());

    let mut pt = sys::Point::default();
    sys::GetCursorPos(&mut pt);
    // 菜单要求前台窗口，否则点菜单外面不会收起。
    sys::SetForegroundWindow(hwnd);
    let cmd = sys::TrackPopupMenu(
        menu,
        sys::TPM_RIGHTBUTTON | sys::TPM_NONOTIFY | sys::TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        ptr::null(),
    );
    sys::DestroyMenu(menu);
    sys::PostMessageW(hwnd, 0, 0, 0);
    match cmd as usize {
        MENU_SETTINGS => {
            let entry = ENTRY.load(Ordering::Relaxed);
            if entry != 0 {
                ctx.log.line("托盘菜单：设置");
                card::show_settings(
                    entry,
                    &ctx.log,
                    &card_json(&ctx.state, &ctx.hist, &ctx.chg_log, ctx.theme, ctx.cfg.clone()),
                );
            }
        }
        MENU_EXIT => {
            ctx.log.line("托盘菜单：退出");
            quit(ctx, hwnd);
        }
        _ => {}
    }
}

/// 退出清理：托盘图标、定时器、消息循环。托盘「退出」和外部 WM_CLOSE 共用。
unsafe fn quit(ctx: &mut Ctx, hwnd: HWND) {
    tray::remove(hwnd);
    if ctx.tray_icon != 0 {
        sys::DestroyIcon(ctx.tray_icon);
        ctx.tray_icon = 0;
    }
    sys::KillTimer(hwnd, TIMER_TICK);
    sys::KillTimer(hwnd, TIMER_ANIM);
    ctx.anim_timer = false;
    sys::PostQuitMessage(0);
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    if msg == TASKBAR_CREATED.load(Ordering::Relaxed) as u32 {
        let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
        if !ctx.is_null() {
            (*ctx).log.line("任务栏重建（explorer 重启），恢复托盘图标");
            tray::remove(hwnd);
            (*ctx).tray_added = false;
            if (*ctx).cfg.tray {
                let icon = ensure_tray_icon(&mut *ctx);
                if icon != 0 && tray::add(hwnd, icon) {
                    (*ctx).tray_added = true;
                }
            }
            tick(&mut *ctx);
            // explorer 重启后任务栏换了进程，按新 PID 重装事件钩子。
            let tray = TRAY.load(Ordering::Relaxed);
            if tray != 0 {
                install_hooks(tray);
            }
        }
        return 0;
    }
    match msg {
        sys::WM_NCCREATE => {
            let cs = l as *const CreateStructW;
            if !cs.is_null() && !(*cs).create_params.is_null() {
                sys::SetWindowLongPtrW(hwnd, sys::GWLP_USERDATA, (*cs).create_params as isize);
            }
            sys::DefWindowProcW(hwnd, msg, w, l)
        }
        // 外部请求退出（安装/卸载程序用）：与托盘「退出」走同一套清理。
        sys::WM_CLOSE => {
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                (*ctx).log.line("收到外部关闭请求，退出");
                quit(&mut *ctx, hwnd);
            }
            0
        }
        sys::WM_TIMER if w == TIMER_TEMP_TRAY => {
            sys::KillTimer(hwnd, TIMER_TEMP_TRAY);
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() && (*ctx).temp_tray {
                (*ctx).temp_tray = false;
                if !(*ctx).cfg.tray {
                    tray::remove(hwnd);
                    (*ctx).tray_added = false;
                    (*ctx).log.line("临时托盘图标已移除");
                }
            }
            0
        }
        sys::WM_TIMER if w == TIMER_TICK => {
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                (*ctx).tick = (*ctx).tick.wrapping_add(1);
                // 全量枚举只在这里按 ≥2.5s 的节奏推进，不受任务栏事件频率影响。
                maybe_refresh_devices(&mut *ctx);
                tick(&mut *ctx);
            }
            0
        }
        sys::WM_TIMER if w == TIMER_ANIM => {
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                anim_frame(&mut *ctx);
            }
            0
        }
        sys::WM_TIMER | WM_APP_REPOSITION | WM_APP_REBUILD => {
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                tick(&mut *ctx);
            }
            0
        }
        WM_APP_CLICK => {
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                // w=1 来自第二实例唤起：卡片已开就保持，不反切关掉。
                if w != 1 || !card::is_open() {
                    (*ctx).log.line("入口被点击");
                    toggle_panel(&mut *ctx);
                }
            }
            0
        }
        WM_APP_TRAY => {
            let ev = (l & 0xFFFF) as u32;
            // 左键点击不做任何反应；只有右键弹出菜单。
            if ev == sys::WM_RBUTTONUP {
                let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
                if !ctx.is_null() {
                    (*ctx).log.line("托盘图标右键");
                    show_tray_menu(hwnd, &mut *ctx);
                }
            }
            0
        }
        sys::WM_SETTINGCHANGE => {
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                if resolve_theme(&mut *ctx) {
                    (*ctx).last_snap = None;
                    (*ctx).log.line("系统主题变更，入口配色更新");
                }
                tick(&mut *ctx);
            }
            0
        }
        WM_APP_CONFIG_CHANGED => {
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                tick(&mut *ctx);
            }
            0
        }
        card::WM_APP_CARD_CFG => {
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                if let Some(json) = card::take_cfg() {
                    apply_card_cfg(&mut *ctx, &json);
                }
            }
            0
        }
        WM_APP_RECONNECT => {
            let ctx = sys::GetWindowLongPtrW(hwnd, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                (*ctx)
                    .log
                    .line("重新连接：重新枚举设备并重开只读句柄（不发查询指令）");
                // 先让旧线程全部退役（CancelSynchronousIo 打断阻塞读），等它们真正
                // 退出后再开新句柄；否则每重连一次就会多留一组线程和句柄。
                for entry in (*ctx).readers.iter_mut() {
                    entry.retiring = true;
                    entry.reader.stop();
                }
                (*ctx).reader_retry.clear();
                (*ctx).last_enum = Some(Instant::now());
                refresh_devices(&mut *ctx);
            }
            0
        }
        _ => sys::DefWindowProcW(hwnd, msg, w, l),
    }
}

/// 最近一次因任务栏事件而投递重定位的时间（200ms 去抖）。
static LAST_REPOSITION: Mutex<Option<Instant>> = Mutex::new(None);
static HOOK_LOC: AtomicIsize = AtomicIsize::new(0);
static HOOK_SHOW: AtomicIsize = AtomicIsize::new(0);

/// 安装/重装 WinEvent 钩子：只订阅任务栏进程，并在 explorer 重启后按新 PID 重装。
unsafe fn install_hooks(tray: HWND) {
    let flags = sys::WINEVENT_OUTOFCONTEXT | sys::WINEVENT_SKIPOWNPROCESS;
    let old_loc = HOOK_LOC.swap(0, Ordering::Relaxed);
    if old_loc != 0 {
        sys::UnhookWinEvent(old_loc);
    }
    let old_show = HOOK_SHOW.swap(0, Ordering::Relaxed);
    if old_show != 0 {
        sys::UnhookWinEvent(old_show);
    }

    // 全系统订阅会让每次窗口移动都回调，这里按任务栏进程过滤。
    let mut pid: u32 = 0;
    if tray != 0 {
        sys::GetWindowThreadProcessId(tray, &mut pid);
    }
    let loc = sys::SetWinEventHook(
        sys::EVENT_OBJECT_LOCATIONCHANGE,
        sys::EVENT_OBJECT_LOCATIONCHANGE,
        0,
        Some(win_event_proc),
        pid,
        0,
        flags,
    );
    let show = sys::SetWinEventHook(
        sys::EVENT_OBJECT_SHOW,
        sys::EVENT_OBJECT_HIDE,
        0,
        Some(win_event_proc),
        pid,
        0,
        flags,
    );
    HOOK_LOC.store(loc, Ordering::Relaxed);
    HOOK_SHOW.store(show, Ordering::Relaxed);
    if loc == 0 && show == 0 {
        let host = HOST.load(Ordering::Relaxed);
        if host != 0 {
            let ctx = sys::GetWindowLongPtrW(host, sys::GWLP_USERDATA) as *mut Ctx;
            if !ctx.is_null() {
                (*ctx)
                    .log
                    .line("SetWinEventHook 失败，仅靠 500ms 定时器跟随任务栏");
            }
        }
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: isize,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if hwnd != TRAY.load(Ordering::Relaxed) {
        return;
    }
    // 任务栏一次布局变化可能连发多个事件：200ms 内只投递一次重定位。
    {
        let mut last = match LAST_REPOSITION.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let now = Instant::now();
        if let Some(t) = *last {
            if now.saturating_duration_since(t) < Duration::from_millis(200) {
                return;
            }
        }
        *last = Some(now);
    }
    let host = HOST.load(Ordering::Relaxed);
    if host != 0 {
        sys::PostMessageW(host, WM_APP_REPOSITION, 0, 0);
    }
}

unsafe fn register_class(name: &str, proc_: sys::WndProc) {
    let wide = sys::wide(name);
    let wc = sys::WndClassW {
        style: 0,
        lpfn_wnd_proc: proc_ as usize as isize,
        cls_extra: 0,
        wnd_extra: std::mem::size_of::<isize>() as i32,
        h_instance: sys::GetModuleHandleW(ptr::null()),
        h_icon: 0,
        h_cursor: sys::LoadCursorW(0, sys::IDC_ARROW as *const u16),
        hbr_background: 0,
        menu_name: ptr::null(),
        class_name: wide.as_ptr(),
    };
    sys::RegisterClassW(&wc);
}

pub fn run(opts: Options) {
    unsafe {
        if sys::SetProcessDpiAwarenessContext(sys::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0
        {
            sys::SetProcessDPIAware();
        }

        // 单实例：同名互斥体已存在则直接退出，避免出现第二个入口。
        let mutex_name = sys::wide(MUTEX_NAME);
        let mutex = sys::CreateMutexW(ptr::null_mut(), 0, mutex_name.as_ptr());
        if mutex == 0 {
            return;
        }
        if std::io::Error::last_os_error().raw_os_error() == Some(183) {
            // 已有实例在跑：让它唤出卡片（只开不切）再退出，不要静默消失。
            let class = sys::wide(HOST_CLASS);
            let host = sys::FindWindowW(class.as_ptr(), ptr::null());
            if host != 0 {
                sys::PostMessageW(host, WM_APP_CLICK, 1, 0);
            }
            return;
        }

        let mut token: usize = 0;
        let input = sys::GdiplusStartupInput {
            gdiplus_version: 1,
            debug_event_callback: ptr::null_mut(),
            suppress_background_thread: 0,
            suppress_external_codecs: 0,
        };
        if sys::GdiplusStartup(&mut token, &input, ptr::null_mut()) != 0 {
            eprintln!("GDI+ 初始化失败");
            return;
        }

        let log = Log::new(opts.log);
        log.line("启动（B 路线：任务栏子窗口入口）");

        let (cfg, autostart_explicit) = Config::load_ex();
        if let Err(e) = cfg.save() {
            log.line(&format!("配置写入失败：{e}"));
        }
        log.line(&format!("配置文件：{}", Config::path().display()));
        log.line(&format!("配置：{}", config::describe(&cfg)));
        config::sync_autostart(&cfg, autostart_explicit, &log);

        let config = Arc::new(Mutex::new(cfg.clone()));
        let state = Arc::new(Mutex::new(AppState::new()));
        // 启动时键鼠可能还在休眠、一帧都没吐过：先用上次落盘的读数顶着，等新帧到了再覆盖。
        let saved = load_saved();
        let hist = load_hist();
        if let Ok(mut st) = state.lock() {
            st.keyboard.battery = st.keyboard.battery.or(saved[0]);
            st.mouse.battery = st.mouse.battery.or(saved[1]);
        }
        if saved[0].is_some() || saved[1].is_some() {
            log.line(&format!(
                "上次读数：键盘 {}，鼠标 {}（休眠时先用它显示）",
                saved[0].map(|v| format!("{v}%")).unwrap_or_else(|| "--".to_string()),
                saved[1].map(|v| format!("{v}%")).unwrap_or_else(|| "--".to_string()),
            ));
        }
        let ctx = Box::new(Ctx {
            state: state.clone(),
            log,
            config,
            cfg: cfg.clone(),
            threshold: cfg.threshold,
            readers: Vec::new(),
            reader_retry: HashMap::new(),
            last_enum: None,
            enum_failed: false,
            last_geo: None,
            last_paint: None,
            last_snap: None,
            last_link: [None; 2],
            last_charge: [Charge::Unknown; 2],
            saved,
            hist,
            hour_mark: None,
            chg_log: load_charge(),
            tick: 0,
            retry: 0,
            tray_added: false,
            tray_fail_logged: false,
            temp_tray: false,
            tray_icon: 0,
            tray_icon_px: 0,
            notify_armed: [false; 2],
            entry_surface: paint::Surface::new(),
            theme: cfg.theme.resolve(paint::system_theme()),
            theme_pref: cfg.theme,
            anim: EntryAnim::default(),
            pulse_start: None,
            anim_timer: false,
            last_link_icon: [None; 2],
            last_tip: String::new(),
            card_json: String::new(),
        });
        let ctx_ptr = Box::into_raw(ctx);

        register_class(ENTRY_CLASS, entry_proc);
        register_class(HOST_CLASS, host_proc);

        let taskbar_msg = sys::RegisterWindowMessageW(sys::wide("TaskbarCreated").as_ptr());
        TASKBAR_CREATED.store(taskbar_msg as usize, Ordering::Relaxed);

        // 宿主窗口带 WS_EX_TOOLWINDOW，能收到 TaskbarCreated 广播（explorer 重启恢复托盘）。
        let host_class = sys::wide(HOST_CLASS);
        let host = sys::CreateWindowExW(
            sys::WS_EX_TOOLWINDOW,
            host_class.as_ptr(),
            host_class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            sys::GetModuleHandleW(ptr::null()),
            ctx_ptr as *mut c_void,
        );
        if host == 0 {
            (*ctx_ptr).log.line("创建宿主窗口失败");
            return;
        }
        HOST.store(host, Ordering::Relaxed);

        let tray_class = sys::wide(TRAY_CLASS);
        let tray = sys::FindWindowW(tray_class.as_ptr(), ptr::null());
        if tray != 0 {
            TRAY.store(tray, Ordering::Relaxed);
        }

        refresh_devices(&mut *ctx_ptr);
        (*ctx_ptr).last_enum = Some(Instant::now());
        tick(&mut *ctx_ptr);

        sys::SetTimer(host, TIMER_TICK, 500, 0);

        install_hooks(TRAY.load(Ordering::Relaxed));

        let mut msg = Msg::default();
        while sys::GetMessageW(&mut msg, 0, 0, 0) > 0 {
            sys::TranslateMessage(&msg);
            sys::DispatchMessageW(&msg);
        }

        let hook_loc = HOOK_LOC.swap(0, Ordering::Relaxed);
        if hook_loc != 0 {
            sys::UnhookWinEvent(hook_loc);
        }
        let hook_show = HOOK_SHOW.swap(0, Ordering::Relaxed);
        if hook_show != 0 {
            sys::UnhookWinEvent(hook_show);
        }
        if (*ctx_ptr).tray_icon != 0 {
            sys::DestroyIcon((*ctx_ptr).tray_icon);
            (*ctx_ptr).tray_icon = 0;
        }
        sys::GdiplusShutdown(token);
    }
}
