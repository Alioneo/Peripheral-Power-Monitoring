//! 应用状态 + 只读采集线程（纯内核，不含任何界面依赖）。
//!
//! 状态机：每台设备独立维护 `Live（使用中）/ Dormant（休眠）/ Absent（未连接）`，
//! 休眠保留最后一次读数与外观；只有“枚举与读取句柄都失效且超过宽限期”才是未连接。
//! 充电态也是每台设备各自做“连续 2 帧确认”的防抖，互不影响。

use crate::log::Log;
use crate::protocol::{self, Charge, Reading};
use crate::sys;
use crate::win;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 鼠标 2.4G 接收器。
pub const MOUSE_PID: u16 = 0x14A7;
/// 鼠标 USB 有线模式（实测镜像同样的 0x07/19 字节帧）。
pub const MOUSE_PID_WIRED: u16 = 0x46A7;
pub const KEYBOARD_PID: u16 = 0x1924;
/// 键盘 USB 有线模式（实测 0xBD 前缀、9 字节帧，电量/充电位与 2.4G 同义）。
pub const KEYBOARD_PID_WIRED: u16 = 0x4824;
pub const RAPOO_VID: u16 = 0x24AE;
pub const RAPOO_USAGE_PAGE: u16 = 0xFF00;
pub const RAPOO_USAGE: u16 = 0x0002;

/// 设备不在枚举结果里之后，仍保留读数的时间；超过才判定为“未连接”。
pub const ABSENT_GRACE: Duration = Duration::from_secs(300);
/// 充电态连续多少帧一致才生效（每台设备各自计数）。
pub const CHARGE_CONFIRM_FRAMES: u8 = 2;
/// 新来源要连续命中这么多帧（或候选持续这么久）才取代当前来源，
/// 防止 2.4G / 有线两路交替发帧时图标来回翻转。
pub const SOURCE_SWITCH_FRAMES: u8 = 2;
pub const SOURCE_SWITCH_HOLD: Duration = Duration::from_millis(500);
/// “来源切换”日志的最小间隔：两路频繁交替时不刷屏。
const SOURCE_SWITCH_LOG_GAP: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Mouse,
    Keyboard,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Mouse => "鼠标",
            Kind::Keyboard => "键盘",
        }
    }

    /// 该设备所有可能供数的 PID（鼠标/键盘：2.4G 与有线各一路）。
    pub fn pids(self) -> &'static [u16] {
        match self {
            Kind::Mouse => &[MOUSE_PID, MOUSE_PID_WIRED],
            Kind::Keyboard => &[KEYBOARD_PID, KEYBOARD_PID_WIRED],
        }
    }

    /// 该设备 USB 有线口的 PID（没有有线通道时为 `None`）。
    pub fn wired_pid(self) -> Option<u16> {
        match self {
            Kind::Mouse => Some(MOUSE_PID_WIRED),
            Kind::Keyboard => Some(KEYBOARD_PID_WIRED),
        }
    }

    pub fn collection(self) -> u16 {
        match self {
            Kind::Mouse => 9,
            Kind::Keyboard => 3,
        }
    }

    /// 无帧多久算休眠（不算未连接）。鼠标空闲实测有 20 秒空档，故 30 秒；
    /// 键盘心跳约 5 秒一帧，休眠/充电时实测空档可达 10 秒，20 秒足够。
    pub fn timeout_secs(self) -> u64 {
        match self {
            Kind::Mouse => 30,
            Kind::Keyboard => 20,
        }
    }
}

/// 对外连接状态（入口与面板共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// 最近一帧在超时时间内。
    Live,
    /// 有历史读数但暂时没有新帧（设备省电休眠）：数值与外观都保留，不变灰。
    Dormant,
    /// 确认断开（不在枚举里且读取句柄失效，并且超过宽限期）。
    Absent,
}

/// 单设备对外状态（入口与后续面板共用）。
#[derive(Clone, Copy)]
pub struct DeviceView {
    pub battery: Option<u8>,
    pub charge: Charge,
    pub last_frame: Option<Instant>,
    /// 最近一次枚举结果里是否在场。
    pub present: bool,
    /// 最近一次“还活着”的证据（收到帧或枚举命中）。
    pub alive_at: Option<Instant>,
    /// 最近一帧的本机时间（时、分、秒），供面板显示“最近更新”。
    pub last_hms: Option<(u16, u16, u16)>,
    /// 最近一帧由哪一路（PID）提供；`None` 表示还没收到过帧。
    /// 只由“真正收到帧”的采集线程写入，因此自然满足“最近收到帧的一路为准”。
    pub source_pid: Option<u16>,
    /// 充电态防抖：候选值与连续帧数。
    pending: Option<Charge>,
    pending_count: u8,
    /// 来源切换滞回：候选来源、连续命中帧数与首次命中时间。
    cand_pid: Option<u16>,
    cand_count: u8,
    cand_since: Option<Instant>,
    /// 最近一次“来源切换”日志时间，用于限频。
    last_switch_log: Option<Instant>,
}

impl DeviceView {
    /// 最近一次供数是否来自该设备的 USB 有线口。
    pub fn wired(&self, kind: Kind) -> bool {
        self.source_pid == kind.wired_pid()
    }
}

impl Default for DeviceView {
    fn default() -> Self {
        DeviceView {
            battery: None,
            charge: Charge::Unknown,
            last_frame: None,
            present: false,
            alive_at: None,
            last_hms: None,
            source_pid: None,
            pending: None,
            pending_count: 0,
            cand_pid: None,
            cand_count: 0,
            cand_since: None,
            last_switch_log: None,
        }
    }
}

pub struct AppState {
    pub mouse: DeviceView,
    pub keyboard: DeviceView,
    pub started_at: Instant,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            mouse: DeviceView::default(),
            keyboard: DeviceView::default(),
            started_at: Instant::now(),
        }
    }

    fn slot(&self, kind: Kind) -> &DeviceView {
        match kind {
            Kind::Mouse => &self.mouse,
            Kind::Keyboard => &self.keyboard,
        }
    }

    fn slot_mut(&mut self, kind: Kind) -> &mut DeviceView {
        match kind {
            Kind::Mouse => &mut self.mouse,
            Kind::Keyboard => &mut self.keyboard,
        }
    }

    /// 面板按设备取状态用。
    #[allow(dead_code)]
    pub fn view(&self, kind: Kind) -> DeviceView {
        *self.slot(kind)
    }

    /// 计算连接状态：休眠与未连接严格分开。
    pub fn link(&self, kind: Kind) -> Link {
        let v = self.slot(kind);
        if let Some(t) = v.last_frame {
            if t.elapsed() <= Duration::from_secs(kind.timeout_secs()) {
                return Link::Live;
            }
        }
        if !v.present {
            let since = v.alive_at.unwrap_or(self.started_at);
            if since.elapsed() >= ABSENT_GRACE {
                return Link::Absent;
            }
        }
        Link::Dormant
    }

    /// 更新在场状态。`in_enum` 是枚举是否命中，`reader_alive` 是采集线程是否仍在阻塞读。
    ///
    /// 休眠中的设备可能一时从枚举结果里消失，但句柄仍停在阻塞读上——这只能说明设备
    /// 在省电，不能说明它被拔掉。因此“在场”取两者的并集，`alive_at` 也随之续期，
    /// 避免休眠久了被宽限期误判成未连接。
    pub fn set_present(&mut self, kind: Kind, in_enum: bool, reader_alive: bool) {
        let now = Instant::now();
        let v = self.slot_mut(kind);
        v.present = in_enum || reader_alive;
        if v.present {
            v.alive_at = Some(now);
        }
    }

    /// 有线采集线程已确定退役（枚举里也没有该接口）时清除来源标记，
    /// 让入口/面板回退到 2.4G 外观；电池值与充电态保持不动（休眠仍保留读数）。
    pub fn clear_source(&mut self, kind: Kind, pid: u16) {
        let v = self.slot_mut(kind);
        if v.source_pid == Some(pid) {
            v.source_pid = None;
            v.cand_pid = None;
            v.cand_count = 0;
            v.cand_since = None;
        }
    }

    /// 更新一帧读数。`source_pid` 是这一帧实际来自的接口（2.4G / 有线）。
    ///
    /// 只有真正收到帧才会调用这里，所以“两个 PID 同时在枚举里”时，始终由
    /// 最近发帧的那一路决定数值与图标；不发帧的一路不会卡住或覆盖显示。
    /// 有历史来源时，新来源要连续命中若干帧或持续领先 `SOURCE_SWITCH_HOLD`
    /// 才真正接管（滞回），避免两路交替发帧导致图标抖动。
    /// 返回值是“需要写切换日志”的上一帧来源；返回当前来源表示无需记录。
    fn update(&mut self, kind: Kind, reading: Reading, source_pid: u16) -> Option<u16> {
        let now = Instant::now();
        let prev_source = self.slot(kind).source_pid;
        let mut switched = false;
        let mut log_suppressed = false;
        {
            let v = self.slot_mut(kind);
            if v.source_pid == Some(source_pid) {
                v.cand_pid = None;
                v.cand_count = 0;
                v.cand_since = None;
            } else if v.source_pid.is_none() {
                // 首帧：没有历史来源可直接接管。
                v.source_pid = Some(source_pid);
                v.cand_pid = None;
                v.cand_count = 0;
                v.cand_since = None;
            } else {
                if v.cand_pid == Some(source_pid) {
                    v.cand_count = v.cand_count.saturating_add(1);
                } else {
                    v.cand_pid = Some(source_pid);
                    v.cand_count = 1;
                    v.cand_since = Some(now);
                }
                let held = v
                    .cand_since
                    .map(|t| now.saturating_duration_since(t) >= SOURCE_SWITCH_HOLD)
                    .unwrap_or(false);
                if v.cand_count >= SOURCE_SWITCH_FRAMES || held {
                    v.source_pid = Some(source_pid);
                    v.cand_pid = None;
                    v.cand_count = 0;
                    v.cand_since = None;
                    switched = true;
                    log_suppressed = match v.last_switch_log {
                        Some(t) => now.saturating_duration_since(t) < SOURCE_SWITCH_LOG_GAP,
                        None => false,
                    };
                    if !log_suppressed {
                        v.last_switch_log = Some(now);
                    }
                }
            }

            v.battery = Some(reading.battery);
            v.last_frame = Some(now);
            v.alive_at = Some(now);
            v.last_hms = Some(sys::local_hms());
            // 充电位无效（0x00 等瞬态）不打断已确认状态，也不打断候选连击。
            if reading.charge != Charge::Unknown {
                if reading.charge == v.charge {
                    v.pending = None;
                    v.pending_count = 0;
                } else {
                    if v.pending == Some(reading.charge) {
                        v.pending_count = v.pending_count.saturating_add(1);
                    } else {
                        v.pending = Some(reading.charge);
                        v.pending_count = 1;
                    }
                    if v.pending_count >= CHARGE_CONFIRM_FRAMES {
                        v.charge = reading.charge;
                        v.pending = None;
                        v.pending_count = 0;
                    }
                }
            }
        }

        if switched {
            if log_suppressed {
                // 限频：这段时间内的切换不再写日志，用“无切换”的返回值抑制。
                Some(source_pid)
            } else {
                prev_source
            }
        } else if prev_source.is_none() {
            None
        } else {
            Some(source_pid)
        }
    }
}

/// 从枚举结果里挑出目标集合（优先 usage FF00/0002 + 指定 COL，其次同 PID 的 FF00/0002）。
pub fn find_device(
    devices: &[win::HidDeviceInfo],
    kind: Kind,
    pid: u16,
) -> Option<win::HidDeviceInfo> {
    let is_rapoo_collection = |d: &&win::HidDeviceInfo| {
        d.vendor_id == RAPOO_VID && d.usage_page == RAPOO_USAGE_PAGE && d.usage == RAPOO_USAGE
    };
    devices
        .iter()
        .filter(is_rapoo_collection)
        .find(|d| d.product_id == pid && d.collection == Some(kind.collection()))
        .or_else(|| {
            devices
                .iter()
                .filter(is_rapoo_collection)
                .find(|d| d.product_id == pid)
        })
        .cloned()
}

/// 供数路的人类可读名称（日志用）。
pub fn source_label(pid: u16) -> &'static str {
    match pid {
        MOUSE_PID | KEYBOARD_PID => "2.4G",
        MOUSE_PID_WIRED | KEYBOARD_PID_WIRED => "有线",
        _ => "未知",
    }
}

/// 采集线程的控制块：停止标志与线程句柄放在同一把锁里，
/// 保证“宿主请求停止”与“线程发布句柄”不会交叉丢失。
struct ReaderCtl {
    stop: bool,
    /// 线程自己的真实句柄（由线程复制后发布），宿主用它 CancelSynchronousIo。
    native_thread: isize,
}

/// 采集线程句柄：置位 `stop` 后线程在下一次读返回时退出。
pub struct Reader {
    ctl: Arc<Mutex<ReaderCtl>>,
    alive: Arc<AtomicBool>,
}

impl Reader {
    /// 请求停止，并打断仍挂在 `ReadFile` 上的同步读。
    ///
    /// 只置标志的话，线程会一直阻塞在读上，句柄和线程都回收不了；
    /// `CancelSynchronousIo` 让阻塞的读立刻返回错误，线程随即看到 stop 退出。
    pub fn stop(&self) {
        let handle = {
            let mut c = match self.ctl.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            c.stop = true;
            std::mem::replace(&mut c.native_thread, 0)
        };
        if handle != 0 {
            unsafe {
                sys::CancelSynchronousIo(handle);
                sys::CloseHandle(handle);
            }
        }
    }

    /// 线程是否仍在运行。打开句柄失败等情况下线程会提前退出，
    /// 宿主据此重启采集，而不是一直占着一个已经死掉的槽位。
    pub fn running(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 连续读取失败的次数上限。设备省电/系统恢复时偶发一次取消不算断开；
/// 连续多次失败才认定句柄失效，让宿主有机会按“真正断开”处理。
const MAX_READ_ERRORS: u32 = 3;

/// 常驻只读采集线程。每成功解析一帧就更新共享状态。
///
/// `source_pid` 是这条线程所读接口的 PID；状态只在收到帧时更新，
/// 因此两路同时在枚举里时，不发帧的一路不会覆盖另一路的显示。
pub fn spawn_reader(
    kind: Kind,
    info: win::HidDeviceInfo,
    source_pid: u16,
    state: Arc<Mutex<AppState>>,
    log: Log,
) -> Reader {
    let ctl = Arc::new(Mutex::new(ReaderCtl {
        stop: false,
        native_thread: 0,
    }));
    let alive = Arc::new(AtomicBool::new(true));
    let ctl_thread = ctl.clone();
    let alive_thread = alive.clone();
    let stop_thread = ctl.clone();
    std::thread::spawn(move || {
        // 无论从哪条路径退出，都把 alive 置为 false。
        struct AliveGuard(Arc<AtomicBool>);
        impl Drop for AliveGuard {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Relaxed);
            }
        }
        let _guard = AliveGuard(alive_thread);

        // 先把本线程句柄发布给宿主；若发布前已经被要求停止，就直接收摊。
        let native = sys::duplicate_current_thread();
        let already_stopped = {
            let mut c = match ctl_thread.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            if c.stop {
                true
            } else {
                if native != 0 {
                    c.native_thread = native;
                }
                false
            }
        };
        if already_stopped {
            if native != 0 {
                unsafe { sys::CloseHandle(native) };
            }
            return;
        }

        let handle = match win::open(&info.path) {
            Ok(h) => h,
            Err(_) => return,
        };
        let buf_len = if info.input_report_len > 0 {
            info.input_report_len
        } else {
            64
        };
        let mut buf = vec![0u8; buf_len.max(64)];
        let mut errors = 0u32;
        loop {
            let stopped = match stop_thread.lock() {
                Ok(g) => g.stop,
                Err(e) => e.into_inner().stop,
            };
            if stopped {
                return;
            }
            match handle.read_raw(&mut buf) {
                Ok(n) => {
                    // 个别固件会返回 0 字节帧，立即重读会变成忙循环占满一个核。
                    if n == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                    errors = 0;
                    let parsed = match kind {
                        Kind::Mouse => protocol::parse_mouse(&buf[..n]),
                        Kind::Keyboard => protocol::parse_keyboard(&buf[..n]),
                    };
                    if let Some(reading) = parsed {
                        let prev = match state.lock() {
                            Ok(mut st) => st.update(kind, reading, source_pid),
                            Err(_) => Some(source_pid),
                        };
                        if prev != Some(source_pid) {
                            match prev {
                                None => log.line(&format!(
                                    "{} 数据来源：{}（0x{:04X}，首帧）",
                                    kind.label(),
                                    source_label(source_pid),
                                    source_pid
                                )),
                                Some(old) => log.line(&format!(
                                    "{} 数据来源切换：{}（0x{:04X}） -> {}（0x{:04X}），以最近收到帧的一路为准",
                                    kind.label(),
                                    source_label(old),
                                    old,
                                    source_label(source_pid),
                                    source_pid
                                )),
                            }
                        }
                    }
                }
                Err(e) => {
                    // 被宿主的 CancelSynchronousIo 打断：属于正常退役，不算读取失败。
                    let stopped = match stop_thread.lock() {
                        Ok(g) => g.stop,
                        Err(err) => err.into_inner().stop,
                    };
                    if stopped {
                        return;
                    }
                    errors = errors.saturating_add(1);
                    if errors >= MAX_READ_ERRORS {
                        // 句柄真的失效（拔掉/接收器复位）：主动退出，宿主会据 running() 判定。
                        log.line(&format!(
                            "{} 读取句柄失效（连续 {} 次错误：{}），采集线程退出",
                            kind.label(),
                            errors,
                            e
                        ));
                        return;
                    }
                    // 设备被拔掉/句柄失效：短暂退避后重试，同时检查停止标志。
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    });
    Reader { ctl, alive }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(
        battery: Option<u8>,
        charge: Charge,
        frame_age: Option<u64>,
        present: bool,
        alive_age: u64,
    ) -> DeviceView {
        let now = Instant::now();
        DeviceView {
            battery,
            charge,
            last_frame: frame_age.map(|s| now - Duration::from_secs(s)),
            present,
            alive_at: Some(now - Duration::from_secs(alive_age)),
            last_hms: None,
            source_pid: None,
            pending: None,
            pending_count: 0,
            cand_pid: None,
            cand_count: 0,
            cand_since: None,
            last_switch_log: None,
        }
    }

    #[test]
    fn dormant_keeps_last_battery() {
        let mut st = AppState::new();
        // 键盘超时 20 秒：40 秒无帧但节点在位 → 休眠，数值照留。
        st.keyboard = view(Some(34), Charge::Charging, Some(40), true, 1);
        assert_eq!(st.link(Kind::Keyboard), Link::Dormant);
        assert_eq!(st.view(Kind::Keyboard).battery, Some(34));
    }

    #[test]
    fn gone_within_grace_is_dormant() {
        let mut st = AppState::new();
        st.mouse = view(Some(61), Charge::Off, Some(60), false, 60);
        assert_eq!(st.link(Kind::Mouse), Link::Dormant);
    }

    #[test]
    fn gone_past_grace_is_absent() {
        let mut st = AppState::new();
        st.mouse = view(Some(61), Charge::Off, Some(400), false, 400);
        assert_eq!(st.link(Kind::Mouse), Link::Absent);
    }

    #[test]
    fn present_but_silent_is_dormant_not_absent() {
        let mut st = AppState::new();
        st.mouse = view(Some(61), Charge::Off, Some(400), true, 1);
        assert_eq!(st.link(Kind::Mouse), Link::Dormant);
    }

    #[test]
    fn charge_needs_two_consecutive_frames() {
        let mut st = AppState::new();
        st.update(
            Kind::Keyboard,
            Reading {
                battery: 34,
                charge: Charge::Charging,
            },
            KEYBOARD_PID,
        );
        assert_eq!(st.keyboard.charge, Charge::Unknown, "第一帧还不能确认充电");
        st.update(
            Kind::Keyboard,
            Reading {
                battery: 34,
                charge: Charge::Charging,
            },
            KEYBOARD_PID,
        );
        assert_eq!(st.keyboard.charge, Charge::Charging, "第二帧确认充电");
    }

    #[test]
    fn charge_unknown_does_not_break_confirmed_state() {
        let mut st = AppState::new();
        st.update(
            Kind::Mouse,
            Reading {
                battery: 61,
                charge: Charge::Charging,
            },
            MOUSE_PID,
        );
        st.update(
            Kind::Mouse,
            Reading {
                battery: 61,
                charge: Charge::Charging,
            },
            MOUSE_PID,
        );
        st.update(
            Kind::Mouse,
            Reading {
                battery: 61,
                charge: Charge::Unknown,
            },
            MOUSE_PID,
        );
        assert_eq!(st.mouse.charge, Charge::Charging);
        st.update(
            Kind::Mouse,
            Reading {
                battery: 62,
                charge: Charge::Off,
            },
            MOUSE_PID,
        );
        st.update(
            Kind::Mouse,
            Reading {
                battery: 62,
                charge: Charge::Off,
            },
            MOUSE_PID,
        );
        assert_eq!(st.mouse.charge, Charge::Off, "断开充电需同样两帧确认");
    }

    #[test]
    fn charge_per_device_independent() {
        let mut st = AppState::new();
        for _ in 0..2 {
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 61,
                    charge: Charge::Charging,
                },
                MOUSE_PID,
            );
            st.update(
                Kind::Keyboard,
                Reading {
                    battery: 34,
                    charge: Charge::Off,
                },
                KEYBOARD_PID,
            );
        }
        assert_eq!(st.mouse.charge, Charge::Charging);
        assert_eq!(st.keyboard.charge, Charge::Off);
    }

    #[test]
    fn last_frame_source_wins_and_reports_switch() {
        let mut st = AppState::new();
        // 2.4G 先供数。
        assert_eq!(
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 60,
                    charge: Charge::Charging
                },
                MOUSE_PID
            ),
            None,
            "首帧没有可切换的旧来源"
        );
        assert!(!st.mouse.wired(Kind::Mouse));
        // 插线：第一帧只是候选，不立刻翻转（防抖动）。
        assert_eq!(
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 67,
                    charge: Charge::Charging
                },
                MOUSE_PID_WIRED
            ),
            Some(MOUSE_PID_WIRED),
            "单帧不切换"
        );
        assert!(!st.mouse.wired(Kind::Mouse), "候选期仍按 2.4G 外观");
        // 有线路连续第二帧：接管，并报告切换。
        assert_eq!(
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 67,
                    charge: Charge::Charging
                },
                MOUSE_PID_WIRED
            ),
            Some(MOUSE_PID),
            "来源切换要能被宿主记录"
        );
        assert_eq!(st.mouse.battery, Some(67));
        assert!(st.mouse.wired(Kind::Mouse));
        // 只读状态：不发帧的 2.4G 路没有任何写入点，不会覆盖 67%。
        assert_eq!(st.mouse.battery, Some(67));
        // 拔线后 2.4G 恢复发帧：同样要连续两帧才切回。
        st.mouse.last_switch_log = None;
        assert_eq!(
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 65,
                    charge: Charge::Off
                },
                MOUSE_PID
            ),
            Some(MOUSE_PID),
            "单帧不切回"
        );
        assert!(st.mouse.wired(Kind::Mouse), "候选期仍保留有线外观");
        assert_eq!(
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 65,
                    charge: Charge::Off
                },
                MOUSE_PID
            ),
            Some(MOUSE_PID_WIRED)
        );
        assert_eq!(st.mouse.battery, Some(65));
        assert!(!st.mouse.wired(Kind::Mouse));
    }

    #[test]
    fn alternating_sources_do_not_flip() {
        let mut st = AppState::new();
        assert_eq!(
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 60,
                    charge: Charge::Off
                },
                MOUSE_PID
            ),
            None
        );
        // 两路交替各来一帧：候选每次都被重置，永远不接管，图标不抖。
        for _ in 0..5 {
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 61,
                    charge: Charge::Off,
                },
                MOUSE_PID_WIRED,
            );
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 60,
                    charge: Charge::Off,
                },
                MOUSE_PID,
            );
        }
        assert!(!st.mouse.wired(Kind::Mouse), "交替发帧时保持原来源");
        assert_eq!(st.mouse.battery, Some(60));
    }

    #[test]
    fn switch_log_is_rate_limited() {
        let mut st = AppState::new();
        st.update(
            Kind::Mouse,
            Reading {
                battery: 60,
                charge: Charge::Off,
            },
            MOUSE_PID,
        );
        st.update(
            Kind::Mouse,
            Reading {
                battery: 67,
                charge: Charge::Off,
            },
            MOUSE_PID_WIRED,
        );
        assert_eq!(
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 67,
                    charge: Charge::Off
                },
                MOUSE_PID_WIRED
            ),
            Some(MOUSE_PID),
            "首次切换要落日志"
        );
        // 2 秒内再切回：行为照切，但日志被限频（返回值等同“未切换”）。
        st.update(
            Kind::Mouse,
            Reading {
                battery: 65,
                charge: Charge::Off,
            },
            MOUSE_PID,
        );
        assert_eq!(
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 65,
                    charge: Charge::Off
                },
                MOUSE_PID
            ),
            Some(MOUSE_PID),
            "限频期内不再重复写切换日志"
        );
        assert!(!st.mouse.wired(Kind::Mouse));
    }

    #[test]
    fn clear_source_only_when_matching() {
        let mut st = AppState::new();
        st.update(
            Kind::Mouse,
            Reading {
                battery: 67,
                charge: Charge::Charging,
            },
            MOUSE_PID_WIRED,
        );
        // 不是这一路：不能动。
        st.clear_source(Kind::Mouse, MOUSE_PID);
        assert!(st.mouse.wired(Kind::Mouse));
        // 这一路退役：来源清掉，读数与充电态保留。
        st.clear_source(Kind::Mouse, MOUSE_PID_WIRED);
        assert!(!st.mouse.wired(Kind::Mouse));
        assert_eq!(st.mouse.battery, Some(67), "只清来源，不动电池值");
        assert_eq!(st.mouse.charge, Charge::Unknown, "充电防抖中不强行改写");
        assert_eq!(st.link(Kind::Mouse), Link::Live, "最近帧仍在超时窗口内");
    }

    #[test]
    fn same_source_does_not_report_switch() {
        let mut st = AppState::new();
        st.update(
            Kind::Mouse,
            Reading {
                battery: 60,
                charge: Charge::Off,
            },
            MOUSE_PID,
        );
        assert_eq!(
            st.update(
                Kind::Mouse,
                Reading {
                    battery: 61,
                    charge: Charge::Off
                },
                MOUSE_PID
            ),
            Some(MOUSE_PID),
            "同一路连续发帧不算切换"
        );
    }

    #[test]
    fn wired_source_survives_dormant() {
        let mut st = AppState::new();
        st.update(
            Kind::Mouse,
            Reading {
                battery: 67,
                charge: Charge::Charging,
            },
            MOUSE_PID_WIRED,
        );
        // 有线设备休眠：枚举暂时拿不到，但句柄仍在阻塞读。
        st.set_present(Kind::Mouse, false, true);
        let now = Instant::now();
        st.mouse.last_frame = Some(now - Duration::from_secs(600));
        st.mouse.alive_at = Some(now - Duration::from_secs(600));
        assert_eq!(st.link(Kind::Mouse), Link::Dormant);
        assert_eq!(st.mouse.battery, Some(67), "休眠保留读数");
        assert!(
            st.mouse.wired(Kind::Mouse),
            "休眠保留有线标记（图标不回退成电池）"
        );
    }

    #[test]
    fn keyboard_wired_switch_is_kind_scoped() {
        let mut st = AppState::new();
        st.update(
            Kind::Keyboard,
            Reading {
                battery: 47,
                charge: Charge::Charging,
            },
            KEYBOARD_PID,
        );
        assert!(!st.keyboard.wired(Kind::Keyboard));
        // 键盘有线 4824 连续两帧（9 字节帧与 2.4G 同义）：切到有线。
        st.update(
            Kind::Keyboard,
            Reading {
                battery: 48,
                charge: Charge::Charging,
            },
            KEYBOARD_PID_WIRED,
        );
        assert_eq!(
            st.update(
                Kind::Keyboard,
                Reading {
                    battery: 48,
                    charge: Charge::Charging
                },
                KEYBOARD_PID_WIRED
            ),
            Some(KEYBOARD_PID)
        );
        assert_eq!(st.keyboard.battery, Some(48));
        assert!(st.keyboard.wired(Kind::Keyboard));
        // 鼠标槽不受键盘来源影响。
        assert!(!st.mouse.wired(Kind::Mouse));
        assert_eq!(st.mouse.battery, None);
        // 拔线后 2.4G 连续两帧，切回电池图标。
        st.keyboard.last_switch_log = None;
        st.update(
            Kind::Keyboard,
            Reading {
                battery: 47,
                charge: Charge::Off,
            },
            KEYBOARD_PID,
        );
        assert_eq!(
            st.update(
                Kind::Keyboard,
                Reading {
                    battery: 47,
                    charge: Charge::Off
                },
                KEYBOARD_PID
            ),
            Some(KEYBOARD_PID_WIRED)
        );
        assert!(!st.keyboard.wired(Kind::Keyboard));
    }

    #[test]
    fn reader_alive_keeps_presence_when_enumeration_misses() {
        let mut st = AppState::new();
        // 枚举暂时拿不到，但句柄还在阻塞读：属于休眠，不能算离场。
        st.set_present(Kind::Mouse, false, true);
        assert!(st.mouse.present);

        // 句柄真的失效且枚举也不见，才允许走离场计时。
        st.set_present(Kind::Mouse, false, false);
        assert!(!st.mouse.present);

        // 枚举命中（哪怕线程刚重建）也算在场。
        st.set_present(Kind::Mouse, true, false);
        assert!(st.mouse.present);
    }

    #[test]
    fn long_sleep_with_live_handle_is_never_absent() {
        let mut st = AppState::new();
        st.set_present(Kind::Keyboard, false, true);
        // 模拟“睡了很久”：把最后的证据时间推远，但线程仍活着。
        let now = Instant::now();
        st.keyboard.last_frame = Some(now - Duration::from_secs(600));
        st.keyboard.alive_at = Some(now - Duration::from_secs(600));
        assert_eq!(st.link(Kind::Keyboard), Link::Dormant);
        assert_eq!(st.view(Kind::Keyboard).battery, None);
    }

    #[test]
    fn absent_requires_both_enumeration_and_handle_to_be_dead() {
        let mut st = AppState::new();
        st.mouse = view(Some(61), Charge::Off, Some(60), false, 400);
        assert_eq!(
            st.link(Kind::Mouse),
            Link::Absent,
            "枚举+句柄都没了且超宽限"
        );

        st.mouse = view(Some(61), Charge::Off, Some(60), true, 400);
        assert_eq!(
            st.link(Kind::Mouse),
            Link::Dormant,
            "仍在枚举里就只能是休眠（数值保留）"
        );
    }
}
