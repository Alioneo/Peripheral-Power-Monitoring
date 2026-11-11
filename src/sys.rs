//! 界面层 Win32 FFI（user32 / gdi32 / gdiplus / kernel32）。零第三方依赖。
//!
//! 只包含任务栏入口需要的窗口、消息、分层窗口与 GDI+ 绘制声明。
//! HID 相关声明仍在 `win.rs`，两者互不依赖。

#![allow(non_snake_case, non_camel_case_types, dead_code)]
// 类型别名刻意与 Win32 头文件同名（HWND/HDC/...），便于对照 MSDN。
#![allow(clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type HWND = isize;
pub type HDC = isize;
pub type HINSTANCE = isize;
pub type HBITMAP = isize;
pub type HGDIOBJ = isize;
pub type HICON = isize;
pub type HCURSOR = isize;
pub type HBRUSH = isize;
pub type HMENU = isize;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type LRESULT = isize;

// ---- 消息 ----
pub const WM_NCCREATE: u32 = 0x0081;
pub const WM_NCDESTROY: u32 = 0x0082;
pub const WM_CREATE: u32 = 0x0001;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_MOUSEACTIVATE: u32 = 0x0021;
pub const WM_NCHITTEST: u32 = 0x0084;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_DPICHANGED_AFTERPARENT: u32 = 0x02E3;
pub const WM_ACTIVATE: u32 = 0x0006;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_APP: u32 = 0x8000;

pub const WA_INACTIVE: u32 = 0;
pub const VK_ESCAPE: usize = 0x1B;

// ---- 窗口样式 ----
pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_EX_LAYERED: u32 = 0x0008_0000;
pub const WS_EX_NOPARENTNOTIFY: u32 = 0x0000_0004;
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
pub const WS_EX_TOPMOST: u32 = 0x0000_0008;

pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_SHOWWINDOW: u32 = 0x0040;

pub const HWND_MESSAGE: HWND = -3;
pub const HWND_TOP: HWND = 0;
pub const HWND_TOPMOST: HWND = -1;
pub const GWLP_USERDATA: i32 = -21;

pub const SW_HIDE: i32 = 0;
pub const SW_SHOWNA: i32 = 8;

// ---- 托盘 / 通知 ----
pub const NIM_ADD: u32 = 0;
pub const NIM_MODIFY: u32 = 1;
pub const NIM_DELETE: u32 = 2;
pub const NIF_MESSAGE: u32 = 0x1;
pub const NIF_ICON: u32 = 0x2;
pub const NIF_TIP: u32 = 0x4;
pub const NIF_INFO: u32 = 0x10;
pub const NIIF_INFO: u32 = 0x1;
pub const NIIF_WARNING: u32 = 0x2;
pub const IDI_APPLICATION: isize = 32512;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_SETTINGCHANGE: u32 = 0x001A;

// ---- 弹出菜单 ----
pub const MF_STRING: u32 = 0x0000_0000;
pub const MF_SEPARATOR: u32 = 0x0000_0800;
pub const MF_GRAYED: u32 = 0x0000_0001;
pub const MF_CHECKED: u32 = 0x0000_0008;
pub const TPM_RIGHTBUTTON: u32 = 0x0002;
pub const TPM_NONOTIFY: u32 = 0x0080;
pub const TPM_RETURNCMD: u32 = 0x0100;
pub const TPM_LEFTALIGN: u32 = 0x0000;
pub const TPM_BOTTOMALIGN: u32 = 0x0020;

// ---- 注册表 ----
pub type HKEY = isize;
pub const HKEY_CURRENT_USER: HKEY = 0x8000_0001u32 as i32 as isize;
pub const REG_SZ: u32 = 1;
pub const REG_DWORD: u32 = 4;
pub const RRF_RT_REG_SZ: u32 = 2;
pub const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
pub const ERROR_FILE_NOT_FOUND: i32 = 2;
pub const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
pub const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

pub const MONITOR_DEFAULTTONEAREST: u32 = 2;

pub const MA_NOACTIVATE: LRESULT = 3;

// ---- 分层窗口 ----
pub const ULW_ALPHA: u32 = 2;
pub const AC_SRC_OVER: u8 = 0x00;
pub const AC_SRC_ALPHA: u8 = 0x01;
pub const DIB_RGB_COLORS: u32 = 0;
pub const BI_RGB: u32 = 0;
/// PixelFormat32bppPARGB（GDI+ 预乘 alpha，配合 ULW_ALPHA 使用）。
pub const PIXEL_FORMAT_32BPP_PARGB: i32 = 0x000E_200B;
/// PixelFormat32bppARGB（非预乘 alpha，存 PNG 时使用）。
pub const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026_200A;
/// GDI+ 高质量双三次插值（缩放预览图用）。
pub const INTERPOLATIONMODE_HIGHBICUBIC: i32 = 7;
/// GDI+ 最近邻插值（像素级放大预览用）。
pub const INTERPOLATIONMODE_NEAREST_NEIGHBOR: i32 = 5;
/// GDI+ 单位：像素（GdipDrawImageRectRect 的源矩形单位，传 UnitWorld=0 会被拒）。
pub const UNIT_PIXEL: i32 = 2;

// ---- WinEvent ----
pub const EVENT_OBJECT_SHOW: u32 = 0x8002;
pub const EVENT_OBJECT_HIDE: u32 = 0x8003;
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub const WINEVENT_OUTOFCONTEXT: u32 = 0;
pub const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;

pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
pub const IDC_ARROW: isize = 32512;

// ---- 结构体 ----

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Size {
    pub cx: i32,
    pub cy: i32,
}

pub const GW_CHILD: u32 = 5;
pub const GW_OWNER: u32 = 4;

pub type HRGN = isize;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PaintStruct {
    pub hdc: HDC,
    pub erase: i32,
    pub paint_rect: Rect,
    pub restore: i32,
    pub inc_update: i32,
    pub reserved: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MonitorInfo {
    pub cb_size: u32,
    pub rc_monitor: Rect,
    pub rc_work: Rect,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BlendFunction {
    pub blend_op: u8,
    pub blend_flags: u8,
    pub source_constant_alpha: u8,
    pub alpha_format: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BitmapInfoHeader {
    pub size: u32,
    pub width: i32,
    pub height: i32,
    pub planes: u16,
    pub bit_count: u16,
    pub compression: u32,
    pub size_image: u32,
    pub x_pels_per_meter: i32,
    pub y_pels_per_meter: i32,
    pub clr_used: u32,
    pub clr_important: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BitmapInfo {
    pub header: BitmapInfoHeader,
    pub colors: [u32; 1],
}

pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// NOTIFYICONDATAW（Vista 之后的完整版，cbSize 用 size_of 填入）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NotifyIconDataW {
    pub cb_size: u32,
    pub hwnd: HWND,
    pub uid: u32,
    pub flags: u32,
    pub callback_message: u32,
    pub h_icon: HICON,
    pub tip: [u16; 128],
    pub state: u32,
    pub state_mask: u32,
    pub info: [u16; 256],
    pub timeout_or_version: u32,
    pub info_title: [u16; 64],
    pub info_flags: u32,
    pub guid_item: [u8; 16],
    pub h_balloon_icon: HICON,
}

impl NotifyIconDataW {
    pub fn new(hwnd: HWND, uid: u32, callback_message: u32) -> Self {
        NotifyIconDataW {
            cb_size: std::mem::size_of::<NotifyIconDataW>() as u32,
            hwnd,
            uid,
            flags: 0,
            callback_message,
            h_icon: 0,
            tip: [0; 128],
            state: 0,
            state_mask: 0,
            info: [0; 256],
            timeout_or_version: 0,
            info_title: [0; 64],
            info_flags: 0,
            guid_item: [0; 16],
            h_balloon_icon: 0,
        }
    }
}

// 与 SDK shellapi.h（NTDDI_VISTA 之后的完整版 NOTIFYICONDATAW）逐字段对齐：
// x64 下 szTip[128] 起于偏移 40、dwState 起于 296，整体 976 字节。
const _: () = {
    assert!(std::mem::size_of::<NotifyIconDataW>() == 976);
    assert!(std::mem::offset_of!(NotifyIconDataW, tip) == 40);
    assert!(std::mem::offset_of!(NotifyIconDataW, state) == 296);
    assert!(std::mem::offset_of!(NotifyIconDataW, info) == 304);
    assert!(std::mem::offset_of!(NotifyIconDataW, timeout_or_version) == 816);
    assert!(std::mem::offset_of!(NotifyIconDataW, info_title) == 820);
    assert!(std::mem::offset_of!(NotifyIconDataW, info_flags) == 948);
    assert!(std::mem::offset_of!(NotifyIconDataW, guid_item) == 952);
    assert!(std::mem::offset_of!(NotifyIconDataW, h_balloon_icon) == 968);
};

/// ICONINFO：`CreateIconIndirect` 的输入（32bpp 彩位图 + 1bpp 掩码）。
#[repr(C)]
pub struct IconInfo {
    pub f_icon: i32,
    pub x_hotspot: u32,
    pub y_hotspot: u32,
    pub hbm_mask: HBITMAP,
    pub hbm_color: HBITMAP,
}

/// GDI+ 图像编码器 CLSID（存盘时用）。
#[repr(C)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// PNG 编码器 CLSID：{557CF406-1A04-11D3-9A73-0000F81EF32E}。
pub const PNG_ENCODER_CLSID: Guid = Guid {
    data1: 0x557C_F406,
    data2: 0x1A04,
    data3: 0x11D3,
    data4: [0x9A, 0x73, 0x00, 0x00, 0xF8, 0x1E, 0xF3, 0x2E],
};

/// 小图标（托盘）的标准尺寸索引；随线程 DPI 感知返回 16/20/24 等。
pub const SM_CXSMICON: i32 = 49;
pub const SM_CYSMICON: i32 = 50;

#[repr(C)]
pub struct WndClassW {
    pub style: u32,
    pub lpfn_wnd_proc: isize,
    pub cls_extra: i32,
    pub wnd_extra: i32,
    pub h_instance: HINSTANCE,
    pub h_icon: HICON,
    pub h_cursor: HCURSOR,
    pub hbr_background: HBRUSH,
    pub menu_name: *const u16,
    pub class_name: *const u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Msg {
    pub hwnd: HWND,
    pub message: u32,
    pub w_param: WPARAM,
    pub l_param: LPARAM,
    pub time: u32,
    pub pt: Point,
}

#[repr(C)]
pub struct CreateStructW {
    pub create_params: *mut c_void,
    pub h_instance: HINSTANCE,
    pub h_menu: HMENU,
    pub hwnd_parent: HWND,
    pub cy: i32,
    pub cx: i32,
    pub y: i32,
    pub x: i32,
    pub style: i32,
    pub name: *const u16,
    pub class: *const u16,
    pub ex_style: u32,
}

#[repr(C)]
pub struct GdiplusStartupInput {
    pub gdiplus_version: u32,
    pub debug_event_callback: *mut c_void,
    pub suppress_background_thread: i32,
    pub suppress_external_codecs: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SystemTime {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

pub type WinEventProc = unsafe extern "system" fn(isize, u32, HWND, i32, i32, u32, u32);

// ---- kernel32 ----

/// COM 线程模型：单线程套间（WebView2 要求）。
pub const COINIT_APARTMENTTHREADED: u32 = 0x2;

#[link(name = "ole32")]
unsafe extern "system" {
    pub fn CoInitializeEx(reserved: *const c_void, coinit: u32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetModuleHandleW(name: *const u16) -> HINSTANCE;
    pub fn CreateMutexW(attributes: *mut c_void, owner: i32, name: *const u16) -> isize;
    pub fn GetLocalTime(st: *mut SystemTime);
    pub fn GetCurrentThreadId() -> u32;
    pub fn MoveFileExW(existing: *const u16, new_name: *const u16, flags: u32) -> i32;
    pub fn CloseHandle(object: isize) -> i32;
    pub fn GetCurrentProcess() -> isize;
    pub fn GetCurrentThread() -> isize;
    pub fn DuplicateHandle(
        source_process: isize,
        source_handle: isize,
        target_process: isize,
        target_handle: *mut isize,
        desired_access: u32,
        inherit_handle: i32,
        options: u32,
    ) -> i32;
    /// 打断指定线程上挂起的同步 IO（用于让阻塞在 ReadFile 上的采集线程可退出）。
    pub fn CancelSynchronousIo(thread: isize) -> i32;
}

/// 复制“当前线程”的真实句柄，供别的线程调用 `CancelSynchronousIo`。失败返回 0。
pub fn duplicate_current_thread() -> isize {
    const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;
    unsafe {
        let mut out: isize = 0;
        let ok = DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentThread(),
            GetCurrentProcess(),
            &mut out,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        );
        if ok == 0 {
            0
        } else {
            out
        }
    }
}

// ---- user32 ----

#[link(name = "user32")]
unsafe extern "system" {
    pub fn RegisterClassW(class: *const WndClassW) -> u16;
    pub fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: HWND,
        menu: HMENU,
        instance: HINSTANCE,
        param: *mut c_void,
    ) -> HWND;
    pub fn DefWindowProcW(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT;
    pub fn DestroyWindow(hwnd: HWND) -> i32;
    pub fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: isize) -> isize;
    pub fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
    pub fn GetMessageW(msg: *mut Msg, hwnd: HWND, min: u32, max: u32) -> i32;
    pub fn PeekMessageW(msg: *mut Msg, hwnd: HWND, min: u32, max: u32, remove: u32) -> i32;
    pub fn TranslateMessage(msg: *const Msg) -> i32;
    pub fn DispatchMessageW(msg: *const Msg) -> LRESULT;
    pub fn PostMessageW(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> i32;
    pub fn SetTimer(hwnd: HWND, id: usize, elapse: u32, callback: isize) -> usize;
    pub fn KillTimer(hwnd: HWND, id: usize) -> i32;
    pub fn GetWindowRect(hwnd: HWND, rect: *mut Rect) -> i32;
    pub fn GetClientRect(hwnd: HWND, rect: *mut Rect) -> i32;
    pub fn ClientToScreen(hwnd: HWND, pt: *mut Point) -> i32;
    pub fn GetParent(hwnd: HWND) -> HWND;
    pub fn IsWindow(hwnd: HWND) -> i32;
    pub fn IsWindowVisible(hwnd: HWND) -> i32;
    pub fn GetWindowDpiAwarenessContext(hwnd: HWND) -> isize;
    pub fn GetThreadDpiAwarenessContext() -> isize;
    pub fn SetThreadDpiAwarenessContext(context: isize) -> isize;
    pub fn SetParent(hwnd: HWND, parent: HWND) -> HWND;
    pub fn FindWindowW(class_name: *const u16, window_name: *const u16) -> HWND;
    pub fn GetDC(hwnd: HWND) -> HDC;
    pub fn ReleaseDC(hwnd: HWND, hdc: HDC) -> i32;
    pub fn LoadCursorW(instance: HINSTANCE, name: *const u16) -> HCURSOR;
    pub fn SetWindowPos(
        hwnd: HWND,
        insert_after: HWND,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    pub fn ShowWindow(hwnd: HWND, cmd: i32) -> i32;
    pub fn SetWinEventHook(
        event_min: u32,
        event_max: u32,
        hook: isize,
        proc_: Option<WinEventProc>,
        process_id: u32,
        thread_id: u32,
        flags: u32,
    ) -> isize;
    pub fn UnhookWinEvent(hook: isize) -> i32;
    pub fn GetDpiForWindow(hwnd: HWND) -> u32;
    pub fn SetProcessDpiAwarenessContext(context: isize) -> i32;
    pub fn SetProcessDPIAware() -> i32;
    pub fn UpdateLayeredWindow(
        hwnd: HWND,
        dst: HDC,
        dst_pt: *const Point,
        size: *const Size,
        src: HDC,
        src_pt: *const Point,
        key: u32,
        blend: *const BlendFunction,
        flags: u32,
    ) -> i32;
    pub fn GetForegroundWindow() -> HWND;
    pub fn BeginPaint(hwnd: HWND, ps: *mut PaintStruct) -> HDC;
    pub fn EndPaint(hwnd: HWND, ps: *const PaintStruct) -> i32;
    pub fn InvalidateRect(hwnd: HWND, rect: *const Rect, erase: i32) -> i32;
    pub fn GetWindow(hwnd: HWND, cmd: u32) -> HWND;
    pub fn SetForegroundWindow(hwnd: HWND) -> i32;
    pub fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut u32) -> u32;
    pub fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: i32) -> i32;
    pub fn SetFocus(hwnd: HWND) -> HWND;
    pub fn GetFocus() -> HWND;
    pub fn MonitorFromWindow(hwnd: HWND, flags: u32) -> isize;
    pub fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    pub fn LoadIconW(instance: HINSTANCE, name: *const u16) -> HICON;
    pub fn CreateIconIndirect(info: *const IconInfo) -> HICON;
    /// 从图标资源数据（.ico 图像条目，可为 PNG 压缩）创建 HICON。
    pub fn CreateIconFromResourceEx(
        presbits: *const u8,
        size: u32,
        f_icon: i32,
        ver: u32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> HICON;
    pub fn DestroyIcon(icon: HICON) -> i32;
    pub fn GetSystemMetrics(index: i32) -> i32;
    pub fn RegisterWindowMessageW(name: *const u16) -> u32;
    pub fn CreatePopupMenu() -> HMENU;
    pub fn DestroyMenu(menu: HMENU) -> i32;
    pub fn AppendMenuW(menu: HMENU, flags: u32, id: usize, text: *const u16) -> i32;
    pub fn TrackPopupMenu(
        menu: HMENU,
        flags: u32,
        x: i32,
        y: i32,
        reserved: i32,
        hwnd: HWND,
        rect: *const Rect,
    ) -> i32;
    pub fn GetCursorPos(pt: *mut Point) -> i32;
    pub fn PostQuitMessage(code: i32);
}

// ---- shell32 ----

#[link(name = "shell32")]
unsafe extern "system" {
    pub fn Shell_NotifyIconW(message: u32, data: *const NotifyIconDataW) -> i32;
}

// ---- advapi32（当前用户注册表，无需管理员权限） ----

#[link(name = "advapi32")]
unsafe extern "system" {
    pub fn RegSetKeyValueW(
        hkey: HKEY,
        sub_key: *const u16,
        value_name: *const u16,
        value_type: u32,
        data: *const c_void,
        data_size: u32,
    ) -> i32;
    pub fn RegDeleteKeyValueW(hkey: HKEY, sub_key: *const u16, value_name: *const u16) -> i32;
    pub fn RegGetValueW(
        hkey: HKEY,
        sub_key: *const u16,
        value_name: *const u16,
        flags: u32,
        value_type: *mut u32,
        data: *mut c_void,
        data_size: *mut u32,
    ) -> i32;
}

// ---- dwmapi ----

#[link(name = "dwmapi")]
extern "system" {
    /// Win11 窗口圆角偏好：attr=33（DWMWA_WINDOW_CORNER_PREFERENCE），值 2=DWMWCP_ROUND。
    /// DWM 绘制的圆角自带抗锯齿；Win10 调用会失败，调用方退回区域裁剪。
    pub fn DwmSetWindowAttribute(hwnd: HWND, attr: u32, value: *const c_void, size: u32) -> i32;
}

// ---- gdi32 ----

#[link(name = "gdi32")]
unsafe extern "system" {
    pub fn CreateCompatibleDC(hdc: HDC) -> HDC;
    pub fn DeleteDC(hdc: HDC) -> i32;
    pub fn CreateDIBSection(
        hdc: HDC,
        info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut c_void,
        section: isize,
        offset: u32,
    ) -> HBITMAP;
    pub fn SelectObject(hdc: HDC, obj: HGDIOBJ) -> HGDIOBJ;
    pub fn DeleteObject(obj: HGDIOBJ) -> i32;
    pub fn CreateBitmap(
        width: i32,
        height: i32,
        planes: u32,
        bits_per_pixel: u32,
        bits: *const c_void,
    ) -> HBITMAP;
    pub fn CreateRoundRectRgn(left: i32, top: i32, right: i32, bottom: i32, w: i32, h: i32)
        -> HRGN;
    pub fn SetWindowRgn(hwnd: HWND, region: HRGN, redraw: i32) -> i32;
}

// ---- gdiplus ----

#[link(name = "gdiplus")]
unsafe extern "system" {
    pub fn GdiplusStartup(
        token: *mut usize,
        input: *const GdiplusStartupInput,
        output: *mut c_void,
    ) -> i32;
    pub fn GdiplusShutdown(token: usize);
    pub fn GdipCreateBitmapFromScan0(
        width: i32,
        height: i32,
        stride: i32,
        format: i32,
        scan0: *mut u8,
        bitmap: *mut *mut c_void,
    ) -> i32;
    pub fn GdipGetImageGraphicsContext(image: *mut c_void, graphics: *mut *mut c_void) -> i32;
    /// 绑定窗口 DC（非分层窗口直接画在窗口表面）。
    pub fn GdipCreateFromHDC(hdc: HDC, graphics: *mut *mut c_void) -> i32;
    pub fn GdipDeleteGraphics(graphics: *mut c_void) -> i32;
    pub fn GdipDisposeImage(image: *mut c_void) -> i32;
    pub fn GdipLoadImageFromFile(filename: *const u16, image: *mut *mut c_void) -> i32;
    pub fn GdipGetImageWidth(image: *mut c_void, width: *mut i32) -> i32;
    pub fn GdipGetImageHeight(image: *mut c_void, height: *mut i32) -> i32;
    pub fn GdipSetInterpolationMode(graphics: *mut c_void, mode: i32) -> i32;
    /// 世界变换左乘缩放（order=0 Prepend）。入口超采样绘制用。
    pub fn GdipScaleWorldTransform(graphics: *mut c_void, sx: f32, sy: f32, order: i32) -> i32;
    pub fn GdipDrawImageRectRect(
        graphics: *mut c_void,
        image: *mut c_void,
        dstx: f32,
        dsty: f32,
        dstw: f32,
        dsth: f32,
        srcx: f32,
        srcy: f32,
        srcw: f32,
        srch: f32,
        unit: i32,
        attributes: *mut c_void,
        callback: *mut c_void,
        callback_data: *mut c_void,
    ) -> i32;
    pub fn GdipSetPenStartCap(pen: *mut c_void, cap: i32) -> i32;
    pub fn GdipSetPenEndCap(pen: *mut c_void, cap: i32) -> i32;
    pub fn GdipSetPenLineJoin(pen: *mut c_void, join: i32) -> i32;
    pub fn GdipSetSmoothingMode(graphics: *mut c_void, mode: i32) -> i32;
    pub fn GdipSetPixelOffsetMode(graphics: *mut c_void, mode: i32) -> i32;
    pub fn GdipSetTextRenderingHint(graphics: *mut c_void, hint: i32) -> i32;
    pub fn GdipCreateSolidFill(color: u32, brush: *mut *mut c_void) -> i32;
    pub fn GdipDeleteBrush(brush: *mut c_void) -> i32;
    /// 线性渐变画笔（GpPointF 两个点 + 两色 + 模式）。
    pub fn GdipCreateLineBrush(
        pt1: *const PointF,
        pt2: *const PointF,
        color1: u32,
        color2: u32,
        mode: i32,
        brush: *mut *mut c_void,
    ) -> i32;
    /// PathGradientBrush（基于 path，天然限形状，径向渐变）。
    pub fn GdipCreatePathGradientFromPath(
        path: *mut c_void,
        brush: *mut *mut c_void,
    ) -> i32;
    pub fn GdipSetPathGradientCenterColor(brush: *mut c_void, color: u32) -> i32;
    pub fn GdipSetPathGradientSurroundColorsWithCount(
        brush: *mut c_void,
        colors: *const u32,
        count: *mut i32,
    ) -> i32;
    pub fn GdipFillRectangle(
        graphics: *mut c_void,
        brush: *mut c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> i32;
    pub fn GdipDrawRectangle(
        graphics: *mut c_void,
        pen: *mut c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> i32;
    pub fn GdipFillPath(graphics: *mut c_void, brush: *mut c_void, path: *mut c_void) -> i32;
    pub fn GdipDrawPath(graphics: *mut c_void, pen: *mut c_void, path: *mut c_void) -> i32;
    pub fn GdipDrawLine(
        graphics: *mut c_void,
        pen: *mut c_void,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    ) -> i32;
    pub fn GdipDrawEllipse(
        graphics: *mut c_void,
        pen: *mut c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> i32;
    pub fn GdipFillEllipse(
        graphics: *mut c_void,
        brush: *mut c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> i32;
    pub fn GdipDrawArc(
        graphics: *mut c_void,
        pen: *mut c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        start: f32,
        sweep: f32,
    ) -> i32;
    pub fn GdipAddPathRectangle(path: *mut c_void, x: f32, y: f32, w: f32, h: f32) -> i32;
    pub fn GdipStartPathFigure(path: *mut c_void) -> i32;
    pub fn GdipAddPathLine(path: *mut c_void, x1: f32, y1: f32, x2: f32, y2: f32) -> i32;
    pub fn GdipCreatePath(fill_mode: i32, path: *mut *mut c_void) -> i32;
    pub fn GdipDeletePath(path: *mut c_void) -> i32;
    pub fn GdipAddPathArc(
        path: *mut c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        start: f32,
        sweep: f32,
    ) -> i32;
    pub fn GdipAddPathLine2(path: *mut c_void, points: *const PointF, count: i32) -> i32;
    pub fn GdipClosePathFigure(path: *mut c_void) -> i32;
    pub fn GdipCreatePen1(color: u32, width: f32, unit: i32, pen: *mut *mut c_void) -> i32;
    pub fn GdipDeletePen(pen: *mut c_void) -> i32;
    pub fn GdipCreateFontFamilyFromName(
        name: *const u16,
        collection: *mut c_void,
        family: *mut *mut c_void,
    ) -> i32;
    pub fn GdipDeleteFontFamily(family: *mut c_void) -> i32;
    pub fn GdipCreateFont(
        family: *mut c_void,
        size: f32,
        style: i32,
        unit: i32,
        font: *mut *mut c_void,
    ) -> i32;
    pub fn GdipDeleteFont(font: *mut c_void) -> i32;
    pub fn GdipCreateStringFormat(flags: i32, language: u16, format: *mut *mut c_void) -> i32;
    pub fn GdipDeleteStringFormat(format: *mut c_void) -> i32;
    pub fn GdipSetStringFormatAlign(format: *mut c_void, align: i32) -> i32;
    pub fn GdipSetStringFormatLineAlign(format: *mut c_void, align: i32) -> i32;
    pub fn GdipSetStringFormatFlags(format: *mut c_void, flags: i32) -> i32;
    pub fn GdipDrawString(
        graphics: *mut c_void,
        text: *const u16,
        length: i32,
        font: *mut c_void,
        layout: *const RectF,
        format: *mut c_void,
        brush: *mut c_void,
    ) -> i32;
    /// 把 GDI+ 位图按指定编码器落盘（用于导出预览 PNG）。
    pub fn GdipSaveImageToFile(
        image: *mut c_void,
        filename: *const u16,
        clsid_encoder: *const Guid,
        encoder_params: *mut c_void,
    ) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PointF {
    pub x: f32,
    pub y: f32,
}

// ---- 小工具 ----

pub fn wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

pub fn argb(alpha: u8, rgb: u32) -> u32 {
    ((alpha as u32) << 24) | (rgb & 0x00FF_FFFF)
}

pub fn now_hms() -> String {
    let mut st = SystemTime::default();
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        st.hour, st.minute, st.second, st.millis
    )
}

/// 本机时间（时、分、秒），用于“最近更新时间”显示。
pub fn local_hms() -> (u16, u16, u16) {
    let mut st = SystemTime::default();
    unsafe { GetLocalTime(&mut st) };
    (st.hour, st.minute, st.second)
}
