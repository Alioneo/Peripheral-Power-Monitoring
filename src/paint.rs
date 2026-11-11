//! 任务栏入口的分层窗口绘制（GDI+ → 32bpp PARGB DIB → UpdateLayeredWindow）。

// 绘制辅助函数按「坐标 + 尺寸 + 样式」逐个传参是刻意的，改动直观，不做参数打包。
#![allow(clippy::too_many_arguments)]

use crate::protocol::Charge;
use crate::sys::{self, BitmapInfo, BitmapInfoHeader, BlendFunction, RectF, HWND};
use std::ffi::c_void;
use std::ptr;

// 统一配色常量，后续调整只改这里。
/// 充电标记（每台设备独立判定）。
/// 托盘卡片底色（品牌青，取自产品图）。
pub const TRAY_CARD: u32 = 0x0000_A9CE;

/// 单设备显示状态：休眠保持休眠前的完整外观，只有未连接才变灰、无读数。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum CellStatus {
    Live,
    #[default]
    Dormant,
    Absent,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    pub battery: Option<u8>,
    pub charge: Charge,
    pub status: CellStatus,
    /// 最近一帧是否来自 USB 有线口（鼠标行据此把电池图标换成 USB 接头图标）。
    pub wired: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Snap {
    pub keyboard: Cell,
    pub mouse: Cell,
}

// ---- 主题 ----

/// 界面主题。深色/浅色两套配色都按同一套语义命名，便于逐项替换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// 用户在设置里选择的主题策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemePref {
    #[default]
    Auto,
    Dark,
    Light,
}

impl ThemePref {
    pub fn resolve(self, system: Theme) -> Theme {
        match self {
            ThemePref::Auto => system,
            ThemePref::Dark => Theme::Dark,
            ThemePref::Light => Theme::Light,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ThemePref::Auto => "auto",
            ThemePref::Dark => "dark",
            ThemePref::Light => "light",
        }
    }
}

/// 配色表（全部为 0xAARRGGBB）。
#[derive(Clone, Copy)]
pub struct Pal {
    /// 键盘 / 鼠标主色
    pub k: u32,
    pub m: u32,
    /// 充电
    pub chg: u32,
    /// 入口卡片
    pub card_bd: u32,
    /// 任务栏入口专用（取自设计稿 --tray-* / --dev-ico / --red-lo）
    pub tray1: u32,
    pub tray2: u32,
    pub tray_hi: u32,
    pub dev_ico: u32,
    pub tray_low: u32,
}

pub const PAL_DARK: Pal = Pal {
    k: 0xFF02_ACF4,
    m: 0xFFE7_B25A,
    chg: 0xFF5B_E49B,
    card_bd: 0xFF2C_4A5C,
    tray1: 0xFF10_212D,
    tray2: 0xFF0A_1620,
    tray_hi: 0x12A0_CDE1,
    dev_ico: 0xFFE6_EFF5,
    tray_low: 0xFFFF_8A72,
};

pub const PAL_LIGHT: Pal = Pal {
    k: 0xFF00_74B8,
    m: 0xFFA2_680C,
    chg: 0xFF0E_8A55,
    card_bd: 0xFFB9_CDD8,
    tray1: 0xFFFF_FFFF,
    tray2: 0xFFED_F4F7,
    tray_hi: 0xE6FF_FFFF,
    dev_ico: 0xFF3B_5D6D,
    tray_low: 0xFFC6_3A24,
};

impl Theme {
    pub fn pal(self) -> &'static Pal {
        match self {
            Theme::Dark => &PAL_DARK,
            Theme::Light => &PAL_LIGHT,
        }
    }
}

const LINK_THEME_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const LINK_THEME_VALUE: &str = "SystemUsesLightTheme";

/// 读系统主题（任务栏/开始菜单是否浅色）。读不到时按深色处理。
pub fn system_theme() -> Theme {
    let sub = sys::wide(LINK_THEME_SUBKEY);
    let name = sys::wide(LINK_THEME_VALUE);
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        sys::RegGetValueW(
            sys::HKEY_CURRENT_USER,
            sub.as_ptr(),
            name.as_ptr(),
            sys::RRF_RT_REG_DWORD,
            ptr::null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    if rc == 0 && data == 1 {
        Theme::Light
    } else {
        Theme::Dark
    }
}

/// 入口尺寸与内部排版（全部为物理像素）。
#[derive(Clone, Copy)]
pub struct Metrics {
    pub width: i32,
    pub height: i32,
    /// 内部统一缩放（16→141 的设计基准乘以它）。
    pub u: f32,
    pub stacked: bool,
    pub icon: f32,
    pub gap: f32,
    pub num_w: f32,
    pub link_w: f32,
    pub font: f32,
    pub radius: f32,
    pub pad: f32,
    pub row_h: f32,
    pub cell_w: f32,
    pub sep_w: f32,
}

/// 依据任务栏尺寸、DPI 算出入口尺寸。
///
/// 横向任务栏：固定 141×35 设计稿等比放大（100% 即 141×35），
/// 竖向任务栏：退化为两行「图标 + 数字」的窄条。
pub fn metrics(scale: f32, tray_w: i32, tray_h: i32) -> Metrics {
    let s = scale.max(0.5);
    let margin = (4.0 * s).round().max(1.0);
    if tray_w >= tray_h {
        let avail_h = (tray_h as f32 - 2.0 * margin).max(18.0 * s);
        let avail_w = (tray_w as f32 - 2.0 * margin).max(60.0 * s);
        let u = (35.0 * s).min(avail_h).min(avail_w * 0.7) / 35.0;
        Metrics {
            width: (141.0 * u).round() as i32,
            height: (35.0 * u).round() as i32,
            u,
            stacked: false,
            icon: 15.0 * u,
            gap: 3.0 * u,
            num_w: 24.0 * u,
            link_w: 14.0 * u,
            font: 18.0 * u,
            radius: 10.0 * u,
            pad: 7.0 * u,
            row_h: 35.0 * u,
            cell_w: 59.0 * u,
            sep_w: 7.0 * u,
        }
    } else {
        let width = (tray_w as f32 - 2.0 * margin).max(18.0 * s);
        let u = s.min(width / 40.0);
        let row_h = 16.0 * u;
        Metrics {
            width: width.round() as i32,
            height: (row_h * 2.0 + 8.0 * u).round() as i32,
            u,
            stacked: true,
            icon: 12.0 * u,
            gap: 2.0 * u,
            num_w: width - 6.0 * u,
            link_w: 0.0,
            font: 12.0 * u,
            radius: 8.0 * u,
            pad: 3.0 * u,
            row_h,
            cell_w: width - 6.0 * u,
            sep_w: 0.0,
        }
    }
}

/// 32bpp 顶向下的 DIB + 内存 DC。
struct Canvas {
    mem_dc: sys::HDC,
    dib: sys::HBITMAP,
    old: sys::HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Canvas {
    fn new(w: i32, h: i32) -> Option<Canvas> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let screen = sys::GetDC(0);
            if screen == 0 {
                return None;
            }
            let mem_dc = sys::CreateCompatibleDC(screen);
            sys::ReleaseDC(0, screen);
            if mem_dc == 0 {
                return None;
            }
            let info = BitmapInfo {
                header: BitmapInfoHeader {
                    size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                    width: w,
                    height: -h, // 负数 = 顶向下
                    planes: 1,
                    bit_count: 32,
                    compression: sys::BI_RGB,
                    size_image: 0,
                    x_pels_per_meter: 0,
                    y_pels_per_meter: 0,
                    clr_used: 0,
                    clr_important: 0,
                },
                colors: [0],
            };
            let mut bits: *mut c_void = ptr::null_mut();
            let dib = sys::CreateDIBSection(mem_dc, &info, sys::DIB_RGB_COLORS, &mut bits, 0, 0);
            if dib == 0 || bits.is_null() {
                sys::DeleteDC(mem_dc);
                return None;
            }
            let old = sys::SelectObject(mem_dc, dib);
            Some(Canvas {
                mem_dc,
                dib,
                old,
                bits: bits as *mut u8,
                w,
                h,
            })
        }
    }

    fn clear(&mut self) {
        unsafe {
            ptr::write_bytes(self.bits, 0, (self.w as usize) * (self.h as usize) * 4);
        }
    }

    /// 直通 alpha → 预乘 alpha（RGB *= A / 255），只改半透明像素，不透明像素原样。
    fn premultiply(&self) {
        let n = (self.w as usize) * (self.h as usize);
        unsafe {
            for i in 0..n {
                let q = self.bits.add(i * 4);
                let a = *q.add(3) as u32;
                if a == 255 {
                    continue;
                }
                if a == 0 {
                    *q = 0;
                    *q.add(1) = 0;
                    *q.add(2) = 0;
                    continue;
                }
                for c in 0..3 {
                    let v = (*q.add(c) as u32 * a + 127) / 255;
                    *q.add(c) = v as u8;
                }
            }
        }
    }

    fn present(&self, hwnd: HWND) -> bool {
        self.present_with_alpha(hwnd, 255)
    }

    fn present_with_alpha(&self, hwnd: HWND, alpha: u8) -> bool {
        // GDI+ 往 32bpp DIB 上画的是「直通 alpha」（RGB 不乘 alpha），而
        // UpdateLayeredWindow 要的是「预乘 alpha」。不转换的话圆角/文字的抗锯齿
        // 边缘会变成硬台阶（低 alpha 像素仍带满亮度 RGB），看起来就是锯齿。
        self.premultiply();
        unsafe {
            let screen = sys::GetDC(0);
            if screen == 0 {
                return false;
            }
            let size = sys::Size {
                cx: self.w,
                cy: self.h,
            };
            let src = sys::Point { x: 0, y: 0 };
            let blend = BlendFunction {
                blend_op: sys::AC_SRC_OVER,
                blend_flags: 0,
                source_constant_alpha: alpha,
                alpha_format: sys::AC_SRC_ALPHA,
            };
            let ok = sys::UpdateLayeredWindow(
                hwnd,
                screen,
                ptr::null(),
                &size,
                self.mem_dc,
                &src,
                0,
                &blend,
                sys::ULW_ALPHA,
            );
            sys::ReleaseDC(0, screen);
            ok != 0
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            sys::SelectObject(self.mem_dc, self.old);
            sys::DeleteObject(self.dib);
            sys::DeleteDC(self.mem_dc);
        }
    }
}

struct Painter {
    g: *mut c_void,
    /// 0 表示 Graphics 直接绑定在窗口 DC 上，无需释放位图。
    bmp: *mut c_void,
}

impl Painter {
    fn new(canvas: &Canvas) -> Option<Painter> {
        unsafe {
            let mut bmp: *mut c_void = ptr::null_mut();
            let st = sys::GdipCreateBitmapFromScan0(
                canvas.w,
                canvas.h,
                canvas.w * 4,
                sys::PIXEL_FORMAT_32BPP_PARGB,
                canvas.bits,
                &mut bmp,
            );
            if st != 0 || bmp.is_null() {
                return None;
            }
            let mut g: *mut c_void = ptr::null_mut();
            let st = sys::GdipGetImageGraphicsContext(bmp, &mut g);
            if st != 0 || g.is_null() {
                sys::GdipDisposeImage(bmp);
                return None;
            }
            Self::tune(g);
            Some(Painter { g, bmp })
        }
    }

    fn tune(g: *mut c_void) {
        unsafe {
            sys::GdipSetSmoothingMode(g, 4); // AntiAlias
            sys::GdipSetPixelOffsetMode(g, 2); // HighQuality
            sys::GdipSetTextRenderingHint(g, 4); // AntiAlias
        }
    }

    fn path_round(x: f32, y: f32, w: f32, h: f32, r: f32) -> *mut c_void {
        unsafe {
            let mut path: *mut c_void = ptr::null_mut();
            if sys::GdipCreatePath(0, &mut path) != 0 || path.is_null() {
                return ptr::null_mut();
            }
            let d = r * 2.0;
            sys::GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
            sys::GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
            sys::GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
            sys::GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
            sys::GdipClosePathFigure(path);
            path
        }
    }

    fn stroke_round(&self, x: f32, y: f32, w: f32, h: f32, r: f32, width: f32, color: u32) {
        unsafe {
            let path = Self::path_round(x, y, w, h, r);
            if path.is_null() {
                return;
            }
            let mut pen: *mut c_void = ptr::null_mut();
            if sys::GdipCreatePen1(color, width, 2, &mut pen) == 0 {
                sys::GdipDrawPath(self.g, pen, path);
                sys::GdipDeletePen(pen);
            }
            sys::GdipDeletePath(path);
        }
    }

    fn fill_rect(&self, x: f32, y: f32, w: f32, h: f32, color: u32) {
        unsafe {
            let mut brush: *mut c_void = ptr::null_mut();
            if sys::GdipCreateSolidFill(color, &mut brush) == 0 {
                sys::GdipFillRectangle(self.g, brush, x, y, w, h);
                sys::GdipDeleteBrush(brush);
            }
        }
    }

    /// 圆角 + 线性渐变（mode=1 垂直 0 水平）。shape 用 path_round 裁掉，超出圆角区域不绘制。
    fn fill_round_linear(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        color1: u32,
        color2: u32,
        mode: i32,
    ) {
        unsafe {
            let path = Self::path_round(x, y, w, h, r);
            if path.is_null() {
                return;
            }
            let pt1 = sys::PointF { x, y };
            let pt2 = if mode == 1 {
                sys::PointF { x, y: y + h }
            } else {
                sys::PointF { x: x + w, y }
            };
            let mut brush: *mut c_void = ptr::null_mut();
            if sys::GdipCreateLineBrush(&pt1, &pt2, color1, color2, mode, &mut brush) == 0 {
                sys::GdipFillPath(self.g, brush, path);
                sys::GdipDeleteBrush(brush);
            }
            sys::GdipDeletePath(path);
        }
    }

    fn text(&self, text: &str, font: *mut c_void, color: u32, rect: RectF, align: i32) {
        unsafe {
            let units: Vec<u16> = text.encode_utf16().collect();
            let mut brush: *mut c_void = ptr::null_mut();
            if sys::GdipCreateSolidFill(color, &mut brush) != 0 {
                return;
            }
            let mut fmt: *mut c_void = ptr::null_mut();
            if sys::GdipCreateStringFormat(0, 0, &mut fmt) == 0 {
                sys::GdipSetStringFormatAlign(fmt, align);
                sys::GdipSetStringFormatLineAlign(fmt, 1); // Center
                // 单行不折行、不裁剪：三位数（100%）比 24px 数字槽宽一点点，
                // 折行会让它在槽里变成两行、整体上移错位。
                sys::GdipSetStringFormatFlags(fmt, 0x1000 | 0x4000); // NoWrap | NoClip
                sys::GdipDrawString(
                    self.g,
                    units.as_ptr(),
                    units.len() as i32,
                    font,
                    &rect,
                    fmt,
                    brush,
                );
                sys::GdipDeleteStringFormat(fmt);
            }
            sys::GdipDeleteBrush(brush);
        }
    }

    fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: u32) {
        unsafe {
            let mut pen: *mut c_void = ptr::null_mut();
            if sys::GdipCreatePen1(color, width, 2, &mut pen) == 0 {
                sys::GdipDrawLine(self.g, pen, x1, y1, x2, y2);
                sys::GdipDeletePen(pen);
            }
        }
    }

    /// 圆头端点直线（等价 SVG `stroke-linecap="round"`）。
    fn stroke_cap(&self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: u32) {
        unsafe {
            let mut pen: *mut c_void = ptr::null_mut();
            if sys::GdipCreatePen1(color, width, 2, &mut pen) == 0 {
                sys::GdipSetPenStartCap(pen, 2);
                sys::GdipSetPenEndCap(pen, 2);
                sys::GdipDrawLine(self.g, pen, x1, y1, x2, y2);
                sys::GdipDeletePen(pen);
            }
        }
    }

    /// 圆头端点的圆弧（等价 SVG arc + round linecap）。
    fn arc_cap(&self, x: f32, y: f32, w: f32, h: f32, start: f32, sweep: f32, width: f32, color: u32) {
        unsafe {
            let mut pen: *mut c_void = ptr::null_mut();
            if sys::GdipCreatePen1(color, width, 2, &mut pen) == 0 {
                sys::GdipSetPenStartCap(pen, 2);
                sys::GdipSetPenEndCap(pen, 2);
                sys::GdipDrawArc(self.g, pen, x, y, w, h, start, sweep);
                sys::GdipDeletePen(pen);
            }
        }
    }

    /// 折线描边，圆角连接 + 圆头端点（等价 SVG `stroke-linejoin="round"`）。
    fn stroke_poly(&self, pts: &[(f32, f32)], width: f32, color: u32) {
        if pts.len() < 2 {
            return;
        }
        let v: Vec<sys::PointF> = pts.iter().map(|p| sys::PointF { x: p.0, y: p.1 }).collect();
        unsafe {
            let mut path: *mut c_void = ptr::null_mut();
            if sys::GdipCreatePath(0, &mut path) != 0 || path.is_null() {
                return;
            }
            sys::GdipAddPathLine2(path, v.as_ptr(), v.len() as i32);
            let mut pen: *mut c_void = ptr::null_mut();
            if sys::GdipCreatePen1(color, width, 2, &mut pen) == 0 {
                sys::GdipSetPenStartCap(pen, 2);
                sys::GdipSetPenEndCap(pen, 2);
                sys::GdipSetPenLineJoin(pen, 2);
                sys::GdipDrawPath(self.g, pen, path);
                sys::GdipDeletePen(pen);
            }
            sys::GdipDeletePath(path);
        }
    }

    fn fill_ellipse(&self, x: f32, y: f32, w: f32, h: f32, color: u32) {
        unsafe {
            let mut brush: *mut c_void = ptr::null_mut();
            if sys::GdipCreateSolidFill(color, &mut brush) == 0 {
                sys::GdipFillEllipse(self.g, brush, x, y, w, h);
                sys::GdipDeleteBrush(brush);
            }
        }
    }

    fn fill_poly(&self, pts: &[(f32, f32)], color: u32) {
        if pts.len() < 3 {
            return;
        }
        let v: Vec<sys::PointF> = pts.iter().map(|p| sys::PointF { x: p.0, y: p.1 }).collect();
        unsafe {
            let mut path: *mut c_void = ptr::null_mut();
            if sys::GdipCreatePath(0, &mut path) != 0 || path.is_null() {
                return;
            }
            sys::GdipAddPathLine2(path, v.as_ptr(), v.len() as i32);
            sys::GdipClosePathFigure(path);
            let mut brush: *mut c_void = ptr::null_mut();
            if sys::GdipCreateSolidFill(color, &mut brush) == 0 {
                sys::GdipFillPath(self.g, brush, path);
                sys::GdipDeleteBrush(brush);
            }
            sys::GdipDeletePath(path);
        }
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        unsafe {
            sys::GdipDeleteGraphics(self.g);
            if !self.bmp.is_null() {
                sys::GdipDisposeImage(self.bmp);
            }
        }
    }
}

// ---- 托盘图标（设计稿定稿Ⅳ双环闪电，不随电量/充电/连接变化） ----

/// 加载外部图片，缩放到托盘图标尺寸、圆角青底叠加、导出预览 PNG。
pub fn export_tray_preview_from_image(dir: &str, image_path: &str) -> Result<Vec<String>, String> {
    let mut token: usize = 0;
    let input = sys::GdiplusStartupInput {
        gdiplus_version: 1,
        debug_event_callback: ptr::null_mut(),
        suppress_background_thread: 0,
        suppress_external_codecs: 0,
    };
    unsafe {
        if sys::GdiplusStartup(&mut token, &input, ptr::null_mut()) != 0 {
            return Err("GDI+ 初始化失败".to_string());
        }
    }
    let r = export_from_image_inner(dir, image_path);
    unsafe { sys::GdiplusShutdown(token) };
    r
}

fn export_from_image_inner(dir: &str, image_path: &str) -> Result<Vec<String>, String> {
    let px: i32 = 16;
    let stride: i32 = px * 4;
    let mut bits = vec![0u8; (stride as usize) * (px as usize)];

    // 1. 创建 16×16 PARGB 位图
    let mut bmp: *mut c_void = ptr::null_mut();
    unsafe {
        let st = sys::GdipCreateBitmapFromScan0(
            px,
            px,
            stride,
            sys::PIXEL_FORMAT_32BPP_PARGB,
            bits.as_mut_ptr(),
            &mut bmp,
        );
        if st != 0 || bmp.is_null() {
            return Err(format!("创建 16x16 位图失败（{st}）"));
        }

        // 2. 在上面画圆角青底
        let mut pgfx: *mut c_void = ptr::null_mut();
        sys::GdipGetImageGraphicsContext(bmp, &mut pgfx);
        sys::GdipSetSmoothingMode(pgfx, 4); // SmoothingModeAntiAlias = 4
        sys::GdipSetPixelOffsetMode(pgfx, 1); // PixelOffsetMode_Half = 1

        let k = px as f32 / 16.0;
        let mut path: *mut c_void = ptr::null_mut();
        sys::GdipCreatePath(0, &mut path);
        // 圆角矩形路径：用 4 个圆弧 + 4 条直线拼成
        let r = 3.7 * k;
        sys::GdipAddPathArc(path, 0.0, 0.0, r * 2.0, r * 2.0, 180.0, 90.0);
        sys::GdipAddPathArc(
            path,
            (px as f32) - r * 2.0,
            0.0,
            r * 2.0,
            r * 2.0,
            270.0,
            90.0,
        );
        sys::GdipAddPathArc(
            path,
            (px as f32) - r * 2.0,
            (px as f32) - r * 2.0,
            r * 2.0,
            r * 2.0,
            0.0,
            90.0,
        );
        sys::GdipAddPathArc(
            path,
            0.0,
            (px as f32) - r * 2.0,
            r * 2.0,
            r * 2.0,
            90.0,
            90.0,
        );
        sys::GdipClosePathFigure(path);

        let mut brush: *mut c_void = ptr::null_mut();
        sys::GdipCreateSolidFill(sys::argb(0xFF, TRAY_CARD), &mut brush);
        sys::GdipFillPath(pgfx, brush, path);
        sys::GdipDeleteBrush(brush);
        sys::GdipDeletePath(path);

        // 3. 加载参考图，缩放到 px×px 叠加
        let mut src_img: *mut c_void = ptr::null_mut();
        let wpath = sys::wide(image_path);
        let st = sys::GdipLoadImageFromFile(wpath.as_ptr(), &mut src_img);
        if st != 0 || src_img.is_null() {
            sys::GdipDeleteGraphics(pgfx);
            sys::GdipDisposeImage(bmp);
            return Err(format!("加载参考图失败（{st}）：{image_path}"));
        }
        let mut sw = 0i32;
        let mut sh = 0i32;
        sys::GdipGetImageWidth(src_img, &mut sw);
        sys::GdipGetImageHeight(src_img, &mut sh);

        sys::GdipSetInterpolationMode(pgfx, sys::INTERPOLATIONMODE_HIGHBICUBIC);
        sys::GdipDrawImageRectRect(
            pgfx,
            src_img,
            0.0,
            0.0,
            px as f32,
            px as f32,
            0.0,
            0.0,
            sw as f32,
            sh as f32,
            sys::UNIT_PIXEL,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        );

        sys::GdipDeleteGraphics(pgfx);
        sys::GdipDisposeImage(src_img);
        sys::GdipDisposeImage(bmp);
    }

    // 4. straighten_alpha
    straighten_alpha(bits.as_mut_ptr(), px, px);

    // 5. 复制到 small 向量
    let w = px as usize;
    let small = bits.clone();

    // 6. 4× 最近邻放大
    let big_w = w * 4;
    let mut big = vec![0u8; big_w * big_w * 4];
    for y in 0..big_w {
        for x in 0..big_w {
            let src = ((y / 4) * w + x / 4) * 4;
            let dst = (y * big_w + x) * 4;
            big[dst..dst + 4].copy_from_slice(&small[src..src + 4]);
        }
    }

    let base = std::path::Path::new(dir);
    std::fs::create_dir_all(base).map_err(|e| format!("创建目录失败：{e}"))?;
    let p1 = base.join("rapoo-tray-icon-16px.png");
    let p2 = base.join("rapoo-tray-icon-16px-4x.png");

    save_png(&small, px, px, &p1)?;
    save_png(&big, big_w as i32, big_w as i32, &p2)?;
    Ok(vec![p1.display().to_string(), p2.display().to_string()])
}

/// 设计稿定稿托盘图标（Ⅳ 双环闪电）：Edge 无头按 `icn-sm`（16/20px）与
/// `icn-lg`（≥24px）渲出的多尺寸 PNG 打包为内嵌 ICO，与设计稿逐像素一致。
static TRAY_ICO: &[u8] = include_bytes!("../assets/tray.ico");

/// 从内嵌 ICO 目录取出图像条目的 PNG 字节：优先取不小于 want 的最近尺寸，
/// 都没有则取最大尺寸。
fn ico_entry(want: u32) -> Option<&'static [u8]> {
    let ico = TRAY_ICO;
    if ico.len() < 6 || ico[0] != 0 || ico[1] != 0 || ico[2] != 1 || ico[3] != 0 {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    if ico.len() < 6 + 16 * count {
        return None;
    }
    let mut best: Option<(u32, usize, usize)> = None; // (边长, 偏移, 长度)
    for i in 0..count {
        let e = 6 + 16 * i;
        let w = if ico[e] == 0 { 256 } else { ico[e] as u32 };
        let len = u32::from_le_bytes(ico[e + 8..e + 12].try_into().ok()?) as usize;
        let off = u32::from_le_bytes(ico[e + 12..e + 16].try_into().ok()?) as usize;
        if off + len > ico.len() {
            continue;
        }
        let replace = match best {
            None => true,
            Some((bs, _, _)) => {
                let big_enough = w >= want;
                let best_big_enough = bs >= want;
                (big_enough && (!best_big_enough || w < bs))
                    || (!big_enough && !best_big_enough && w > bs)
            }
        };
        if replace {
            best = Some((w, off, len));
        }
    }
    let (_, off, len) = best?;
    Some(&ico[off..off + len])
}


/// GDI+ 输出为预乘 alpha；作为 HICON / PNG 前必须还原成直通 alpha，
/// 否则浅色任务栏上圆角边缘会出现深色脏边。
fn straighten_alpha(bits: *mut u8, w: i32, h: i32) {
    let n = w as usize * h as usize;
    unsafe {
        for i in 0..n {
            let p = bits.add(i * 4);
            let a = *p.add(3) as u32;
            if a == 0 || a == 255 {
                continue;
            }
            for c in 0..3 {
                let v = ((*p.add(c) as u32) * 255 / a).min(255) as u8;
                *p.add(c) = v;
            }
        }
    }
}

/// 从内嵌 ICO（设计稿渲染的多尺寸 PNG）创建托盘图标；调用方负责 `DestroyIcon`。
pub fn tray_icon(px: i32) -> Option<sys::HICON> {
    let want = px.clamp(8, 256) as u32;
    let data = ico_entry(want)?;
    unsafe {
        let icon = sys::CreateIconFromResourceEx(
            data.as_ptr(),
            data.len() as u32,
            1,
            0x0003_0000,
            want as i32,
            want as i32,
            0,
        );
        if icon == 0 {
            None
        } else {
            Some(icon)
        }
    }
}

fn save_png(bits: &[u8], w: i32, h: i32, path: &std::path::Path) -> Result<(), String> {
    unsafe {
        let mut bmp: *mut c_void = ptr::null_mut();
        let st = sys::GdipCreateBitmapFromScan0(
            w,
            h,
            w * 4,
            sys::PIXEL_FORMAT_32BPP_ARGB,
            bits.as_ptr() as *mut u8,
            &mut bmp,
        );
        if st != 0 || bmp.is_null() {
            return Err(format!("创建 GDI+ 位图失败（{st}）"));
        }
        let name = sys::wide(&path.display().to_string());
        let st =
            sys::GdipSaveImageToFile(bmp, name.as_ptr(), &sys::PNG_ENCODER_CLSID, ptr::null_mut());
        sys::GdipDisposeImage(bmp);
        if st != 0 {
            return Err(format!("保存 PNG 失败（{st}）：{}", path.display()));
        }
    }
    Ok(())
}

/// 导出托盘图标预览：真实尺寸（1:1）与最近邻放大 4 倍两张 PNG。
pub fn export_tray_preview(dir: &str) -> Result<Vec<String>, String> {
    let mut token: usize = 0;
    let input = sys::GdiplusStartupInput {
        gdiplus_version: 1,
        debug_event_callback: ptr::null_mut(),
        suppress_background_thread: 0,
        suppress_external_codecs: 0,
    };
    unsafe {
        if sys::GdiplusStartup(&mut token, &input, ptr::null_mut()) != 0 {
            return Err("GDI+ 初始化失败".to_string());
        }
    }
    let r = export_tray_preview_inner(dir);
    unsafe {
        sys::GdiplusShutdown(token);
    }
    r
}

fn export_tray_preview_inner(dir: &str) -> Result<Vec<String>, String> {
    // 1:1 直接写内嵌 ICO 里的 16px PNG（Edge 按设计稿渲染，逐像素一致）。
    let png = ico_entry(16).ok_or_else(|| "内嵌 ICO 缺少 16px 图像".to_string())?;
    let base = std::path::Path::new(dir);
    std::fs::create_dir_all(base).map_err(|e| format!("创建目录失败：{e}"))?;
    let p1 = base.join("rapoo-tray-icon-16px.png");
    std::fs::write(&p1, png).map_err(|e| format!("写入 PNG 失败：{e}"))?;

    // 4× 最近邻放大（GDI+ 重采样，便于肉眼检查像素）。
    let big: i32 = 16 * 4;
    let stride = big * 4;
    let mut bits = vec![0u8; (stride as usize) * (big as usize)];
    unsafe {
        let mut bmp: *mut c_void = ptr::null_mut();
        let st = sys::GdipCreateBitmapFromScan0(
            big,
            big,
            stride,
            sys::PIXEL_FORMAT_32BPP_PARGB,
            bits.as_mut_ptr(),
            &mut bmp,
        );
        if st != 0 || bmp.is_null() {
            return Err(format!("创建预览位图失败（{st}）"));
        }
        let mut pgfx: *mut c_void = ptr::null_mut();
        sys::GdipGetImageGraphicsContext(bmp, &mut pgfx);
        let wpath = sys::wide(&p1.display().to_string());
        let mut img: *mut c_void = ptr::null_mut();
        let st = sys::GdipLoadImageFromFile(wpath.as_ptr(), &mut img);
        if st != 0 || img.is_null() {
            sys::GdipDeleteGraphics(pgfx);
            sys::GdipDisposeImage(bmp);
            return Err(format!("加载预览源图失败（{st}）"));
        }
        sys::GdipSetInterpolationMode(pgfx, sys::INTERPOLATIONMODE_NEAREST_NEIGHBOR);
        sys::GdipDrawImageRectRect(
            pgfx,
            img,
            0.0,
            0.0,
            big as f32,
            big as f32,
            0.0,
            0.0,
            16.0,
            16.0,
            sys::UNIT_PIXEL,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        );
        sys::GdipDisposeImage(img);
        sys::GdipDeleteGraphics(pgfx);
        sys::GdipDisposeImage(bmp);
    }
    straighten_alpha(bits.as_mut_ptr(), big, big);
    let p2 = base.join("rapoo-tray-icon-16px-4x.png");
    save_png(&bits, big, big, &p2)?;
    Ok(vec![p1.display().to_string(), p2.display().to_string()])
}

/// 数字取字：只有确认未连接才显示 `--`；休眠与使用中一样保留上一次读数。
fn cell_text(status: CellStatus, battery: Option<u8>) -> String {
    match (status, battery) {
        (CellStatus::Absent, _) => "--".to_string(),
        (_, Some(b)) => format!("{b}"),
        (_, None) => "--".to_string(),
    }
}

fn with_alpha(color: u32, k: f32) -> u32 {
    let base = color & 0x00FF_FFFF;
    let a = (color >> 24) as f32 * k.clamp(0.0, 1.0);
    ((a.round() as u32) << 24) | base
}

// ---- 字体缓存：避免每帧创建族/字体（GDI+ 对象创建很贵） ----
thread_local! {
    static FONT_CACHE: std::cell::RefCell<Vec<(String, i32, i32, *mut c_void)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn font_cached(face: &str, size: f32, style: i32) -> *mut c_void {
    let key = (size * 32.0).round() as i32;
    FONT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        for (f, s, st, h) in cache.iter() {
            if f == face && *s == key && *st == style && !h.is_null() {
                return *h;
            }
        }
        unsafe {
            let name = sys::wide(face);
            let mut family: *mut c_void = ptr::null_mut();
            if sys::GdipCreateFontFamilyFromName(name.as_ptr(), ptr::null_mut(), &mut family) != 0
                || family.is_null()
            {
                return ptr::null_mut();
            }
            let mut font: *mut c_void = ptr::null_mut();
            if sys::GdipCreateFont(family, size, style, 2, &mut font) != 0 {
                font = ptr::null_mut();
            }
            sys::GdipDeleteFontFamily(family);
            if cache.len() < 48 && !font.is_null() {
                cache.push((face.to_string(), key, style, font));
            }
            font
        }
    })
}

/// 数字/大字号：Bahnschrift SemiCondensed（实测 24px 槽装得下三位数）。
fn display_font(size: f32, style: i32) -> *mut c_void {
    let f = font_cached("Bahnschrift SemiCondensed", size, style);
    if f.is_null() {
        font_cached("Segoe UI", size, style)
    } else {
        f
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkIcon {
    /// 无线（2.4G）：`link-24` 无线电波。
    #[default]
    Radio,
    Usb,
    Bolt,
}

#[derive(Clone, Copy, Default)]
pub struct EntryAnim {
    /// 充电雷电的呼吸相位（0..1，周期 1.9s）。
    pub pulse: f32,
    /// 是否存在充电设备（需要持续出帧做呼吸）。
    pub pulse_on: bool,
}

impl EntryAnim {
    pub fn busy(&self) -> bool {
        self.pulse_on
    }
}

/// 按真实链路选图标：无线 2.4G -> 无线电波，有线 -> USB-C，充电 -> 雷电（替换链路图标）。
///
/// 未连接时仍显示无线电波（设计稿只把图标与数字降到 0.4 透明度）；蓝牙暂不处理。
pub fn link_icon(cell: Cell) -> LinkIcon {
    if cell.status != CellStatus::Absent && cell.charge == Charge::Charging {
        LinkIcon::Bolt
    } else if cell.status != CellStatus::Absent && cell.wired {
        LinkIcon::Usb
    } else {
        LinkIcon::Radio
    }
}

// ---- 入口字形：全部按设计稿 SVG 的 viewBox 24 坐标原样复刻，乘 size/24 ----

/// `em-kbd`：键盘外框 + 两行键位短划 + 空格键。
fn glyph_kbd(p: &Painter, cx: f32, cy: f32, size: f32, color: u32) {
    let s = size / 24.0;
    let ox = cx - 12.0 * s;
    let oy = cy - 12.0 * s;
    let sw = 1.6 * s;
    p.stroke_round(
        ox + 2.0 * s,
        oy + 5.0 * s,
        20.0 * s,
        14.0 * s,
        2.5 * s,
        sw,
        color,
    );
    for y in [9.0f32, 12.0] {
        for (i, x) in [5.5f32, 9.0, 12.5, 16.0, 18.5].iter().enumerate() {
            let len = if i == 4 { 0.2 } else { 1.0 };
            p.stroke_cap(
                ox + x * s,
                oy + y * s,
                ox + (x + len) * s,
                oy + y * s,
                sw,
                color,
            );
        }
    }
    p.stroke_cap(
        ox + 7.0 * s,
        oy + 15.2 * s,
        ox + 17.0 * s,
        oy + 15.2 * s,
        sw,
        color,
    );
}

/// `em-mse`：鼠标外轮廓 + 中缝 + 滚轮横线。
fn glyph_mse(p: &Painter, cx: f32, cy: f32, size: f32, color: u32) {
    let s = size / 24.0;
    let ox = cx - 12.0 * s;
    let oy = cy - 12.0 * s;
    let sw = 1.6 * s;
    p.stroke_round(
        ox + 6.5 * s,
        oy + 2.5 * s,
        11.0 * s,
        19.0 * s,
        5.5 * s,
        sw,
        color,
    );
    p.stroke_cap(ox + 12.0 * s, oy + 2.5 * s, ox + 12.0 * s, oy + 8.7 * s, sw, color);
    p.stroke_cap(ox + 9.2 * s, oy + 9.2 * s, ox + 14.8 * s, oy + 9.2 * s, sw, color);
}

/// `link-24`：三段上凸圆弧 + 底部实心点（角度由 SVG 端点与半径反算）。
fn glyph_radio(p: &Painter, cx: f32, cy: f32, size: f32, color: u32) {
    let s = size / 24.0;
    let ox = cx - 12.0 * s;
    let oy = cy - 12.0 * s;
    let sw = 1.6 * s;
    for (r, ccy, start, sweep) in [
        (13.5f32, 18.0917f32, 225.28f32, 89.44f32),
        (8.5, 18.2208, 225.10, 89.80),
        (3.6, 18.29, 223.76, 92.48),
    ] {
        p.arc_cap(
            ox + (12.0 - r) * s,
            oy + (ccy - r) * s,
            2.0 * r * s,
            2.0 * r * s,
            start,
            sweep,
            sw,
            color,
        );
    }
    p.fill_ellipse(
        ox + 10.9 * s,
        oy + 17.9 * s,
        2.2 * s,
        2.2 * s,
        color,
    );
}

/// `link-usb`：USB-C 接头。
fn glyph_usb(p: &Painter, cx: f32, cy: f32, size: f32, color: u32) {
    let s = size / 24.0;
    let ox = cx - 12.0 * s;
    let oy = cy - 12.0 * s;
    let sw = 1.6 * s;
    let x = |v: f32| ox + v * s;
    let y = |v: f32| oy + v * s;
    // M12 3v7
    p.stroke_cap(x(12.0), y(3.0), x(12.0), y(10.0), sw, color);
    // M9 5.5V3h6v2.5
    p.stroke_poly(
        &[
            (x(9.0), y(5.5)),
            (x(9.0), y(3.0)),
            (x(15.0), y(3.0)),
            (x(15.0), y(5.5)),
        ],
        sw,
        color,
    );
    // M8 10h8v3 + 两个 a4 4 半圆（圆心 12,13 半径 4）+ Z
    p.stroke_poly(
        &[(x(8.0), y(10.0)), (x(16.0), y(10.0)), (x(16.0), y(13.0))],
        sw,
        color,
    );
    p.arc_cap(x(8.0), y(9.0), 8.0 * s, 8.0 * s, 0.0, 180.0, sw, color);
    p.line(x(8.0), y(13.0), x(8.0), y(10.0), sw, color);
    // M12 17v3M9.5 20h5
    p.stroke_cap(x(12.0), y(17.0), x(12.0), y(20.0), sw, color);
    p.stroke_cap(x(9.5), y(20.0), x(14.5), y(20.0), sw, color);
}

/// `ic-bolt`：实心闪电（槽内 12×16 视口按 meet 缩放后即 12×12 居中）。
fn glyph_bolt(p: &Painter, cx: f32, cy: f32, size: f32, color: u32) {
    let s = size / 24.0;
    let ox = cx - 12.0 * s;
    let oy = cy - 12.0 * s;
    let src = [
        (13.5f32, 2.0f32),
        (4.5, 13.5),
        (10.0, 13.5),
        (9.5, 22.0),
        (18.5, 10.5),
        (13.0, 10.5),
    ];
    let pts: Vec<(f32, f32)> = src.iter().map(|(x, y)| (ox + x * s, oy + y * s)).collect();
    p.fill_poly(&pts, color);
}

/// 数字颜色：低电按设计稿换成 `--red-lo`，其余用本设备主色。
fn num_color(cell: Cell, base: u32, threshold: u8, pal: &Pal) -> u32 {
    if cell.battery.is_some_and(|b| b <= threshold) {
        pal.tray_low
    } else {
        base
    }
}

/// 数字：18px Bahnschrift 600、24px 定宽居中；低电换色，未连接整侧降透明度。
fn draw_num(
    p: &Painter,
    font: *mut c_void,
    font_tight: *mut c_void,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    cell: Cell,
    base: u32,
    threshold: u8,
    pal: &Pal,
    alpha: f32,
) {
    let text = cell_text(cell.status, cell.battery);
    let color = num_color(cell, base, threshold, pal);
    // 三位数（100%）比 24u 的数字槽宽：GDI+ 会按矩形宽截断第三个字，
    // 所以把绘制矩形左右对称放宽（居中不变，视觉位置不动），再配小一号的字。
    let (font, x, w) = if text.chars().count() >= 3 {
        (font_tight, x - w * 0.25, w * 1.5)
    } else {
        (font, x, w)
    };
    p.text(
        &text,
        font,
        with_alpha(color, alpha),
        RectF { x, y, w, h },
        1,
    );
}

/// 链路槽内容：按真实链路（无线/USB-C）；充电时绿色雷电「替换」链路图标并呼吸。
fn glyph_link(
    p: &Painter,
    cell: Cell,
    mouse: bool,
    cx: f32,
    cy: f32,
    size: f32,
    pal: &Pal,
    pulse: f32,
) {
    let base = if mouse { pal.m } else { pal.k };
    match link_icon(cell) {
        LinkIcon::Radio => glyph_radio(p, cx, cy, size, base),
        LinkIcon::Usb => glyph_usb(p, cx, cy, size, base),
        // 设计稿 boltPulse：0/100% 满亮、50% 半透明，1.9s 循环。
        LinkIcon::Bolt => {
            let a = 0.75 + 0.25 * (pulse * std::f32::consts::TAU).cos();
            glyph_bolt(p, cx, cy, size * 12.0 / 14.0, with_alpha(pal.chg, a));
        }
    }
}

/// 竖向任务栏（设计稿无此形态）：退化为两行「设备图标 + 数字」。
fn render_stacked(p: &Painter, m: &Metrics, snap: &Snap, threshold: u8, pal: &Pal) {
    let u = m.u;
    let w = m.width as f32;
    let h = m.height as f32;
    let font = display_font(m.font, 1);
    if font.is_null() {
        return;
    }
    let row_h = m.row_h;
    let y0 = (h - (row_h * 2.0 + 4.0 * u)) / 2.0;
    for (i, (cell, base, mouse)) in [(snap.keyboard, pal.k, false), (snap.mouse, pal.m, true)]
        .iter()
        .enumerate()
    {
        let y = y0 + i as f32 * (row_h + 4.0 * u);
        let alpha = if cell.status == CellStatus::Absent {
            0.4
        } else {
            1.0
        };
        let icon = m.icon.max(10.0 * u);
        let cx = m.pad + icon / 2.0;
        let cy = y + row_h / 2.0;
        if *mouse {
            glyph_mse(p, cx, cy, icon, with_alpha(pal.dev_ico, alpha));
        } else {
            glyph_kbd(p, cx, cy, icon, with_alpha(pal.dev_ico, alpha));
        }
        let tx = m.pad + icon + 2.0 * u;
        draw_num(
            p,
            font,
            font,
            tx,
            y,
            (w - tx - m.pad).max(8.0 * u),
            row_h,
            *cell,
            *base,
            threshold,
            pal,
            alpha,
        );
    }
}

/// 入口绘制表面：尺寸不变时复用同一块 DIB，避免每帧 CreateDIBSection。
/// `hi` 是 SS 倍超采样渲染目标，画完盒式降采样进 `canvas` 再提交，圆角/文字边缘更平滑。
pub struct Surface {
    canvas: Option<Canvas>,
    hi: Option<Canvas>,
    w: i32,
    h: i32,
}

/// 超采样倍数：4× 边缘更柔，开销仍可忽略。
const SS: usize = 4;

impl Surface {
    pub fn new() -> Surface {
        Surface {
            canvas: None,
            hi: None,
            w: 0,
            h: 0,
        }
    }

    fn ensure(&mut self, w: i32, h: i32) -> bool {
        if self.canvas.is_some() && self.w == w && self.h == h {
            return true;
        }
        self.canvas = Canvas::new(w.max(1), h.max(1));
        self.hi = Canvas::new((w.max(1) * SS as i32).max(1), (h.max(1) * SS as i32).max(1));
        self.w = w;
        self.h = h;
        self.canvas.is_some() && self.hi.is_some()
    }
}

/// 盒式降采样：每个目标像素取 SS×SS 源像素四通道算术平均（直通 alpha，交给 present 统一预乘）。
fn downscale_box(src: &Canvas, dst: &mut Canvas) {
    let ss = SS;
    let (dw, dh, sw) = (dst.w as usize, dst.h as usize, src.w as usize);
    let n = (ss * ss) as u32;
    let (s, d) = (src.bits, dst.bits);
    for y in 0..dh {
        for x in 0..dw {
            let mut acc = [0u32; 4];
            for dy in 0..ss {
                for dx in 0..ss {
                    let q = unsafe { s.add(((y * ss + dy) * sw + (x * ss + dx)) * 4) };
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += unsafe { *q.add(c) } as u32;
                    }
                }
            }
            let q = unsafe { d.add((y * dw + x) * 4) };
            for (c, a) in acc.iter().enumerate() {
                unsafe { *q.add(c) = ((a + n / 2) / n) as u8 };
            }
        }
    }
}

/// 画一帧并提交到分层窗口。返回 false 表示 GDI+ 或 ULW 失败。
pub fn paint_entry(
    surface: &mut Surface,
    hwnd: HWND,
    m: &Metrics,
    snap: &Snap,
    threshold: u8,
    pal: &Pal,
    anim: &EntryAnim,
) -> bool {
    if !surface.ensure(m.width, m.height) {
        return false;
    }
    let hi = surface.hi.as_mut().unwrap();
    let canvas = surface.canvas.as_mut().unwrap();
    // 背景层走超采样，只为平滑圆角；内容层 1× 直绘，保持文字/图标原有边缘。
    hi.clear();
    {
        let p = match Painter::new(hi) {
            Some(p) => p,
            None => return false,
        };
        // 逻辑坐标不变，光栅化按 SS 倍采样。
        unsafe { sys::GdipScaleWorldTransform(p.g, SS as f32, SS as f32, 0) };
        render_bg(&p, m, pal);
    }
    // 降采样会写满画布的每个像素，无需先清零。
    downscale_box(hi, canvas);
    {
        let p = match Painter::new(canvas) {
            Some(p) => p,
            None => return false,
        };
        render(&p, m, snap, threshold, pal, anim);
    }
    canvas.present(hwnd)
}

/// 背景层：圆角渐变板 + 边框 + 顶部高光带。只在超采样画布上绘制。
fn render_bg(p: &Painter, m: &Metrics, pal: &Pal) {
    let u = m.u;
    let w = m.width as f32;
    let h = m.height as f32;

    // 背景板：linear-gradient(180deg,#10212D,#0A1620) + 1px 边框 + R10。
    p.fill_round_linear(0.0, 0.0, w, h, m.radius, pal.tray1, pal.tray2, 1);
    p.stroke_round(
        0.5 * u,
        0.5 * u,
        w - u,
        h - u,
        (m.radius - u / 2.0).max(0.0),
        u,
        pal.card_bd,
    );
    // inset 0 1px 0 rgba(160,205,225,.07)：边框内侧顶部的 1px 高光带。
    let hi_x = (m.radius * 0.6).max(u);
    p.fill_rect(hi_x, u, (w - 2.0 * hi_x).max(0.0), u, pal.tray_hi);
}

/// 内容层：图标、数字、链路、分隔线。1× 直绘，边缘观感与超采样前一致。
fn render(p: &Painter, m: &Metrics, snap: &Snap, threshold: u8, pal: &Pal, anim: &EntryAnim) {
    let u = m.u;
    let h = m.height as f32;

    if m.stacked {
        render_stacked(p, m, snap, threshold, pal);
        return;
    }

    let font = display_font(m.font, 1);
    if font.is_null() {
        return;
    }
    // 三位数用的紧凑字（三星数比 24u 数字槽宽，小一号才放得下）。
    let font_tight = display_font(m.font * 0.92, 1);
    let font_tight = if font_tight.is_null() { font } else { font_tight };

    let cy = h / 2.0;
    let x0 = u + m.pad;
    let gap = m.gap;
    // 两组单元：键盘左、鼠标右；组宽 59u（图标+数字+链路槽），中间隔 1u 竖线 + 两侧 3u。
    for (cell, base, mouse, cx) in [
        (snap.keyboard, pal.k, false, x0),
        (snap.mouse, pal.m, true, x0 + m.cell_w + m.sep_w),
    ] {
        let alpha = if cell.status == CellStatus::Absent {
            0.4
        } else {
            1.0
        };
        let dcx = cx + m.icon / 2.0;
        if mouse {
            glyph_mse(p, dcx, cy, m.icon, with_alpha(pal.dev_ico, alpha));
        } else {
            glyph_kbd(p, dcx, cy, m.icon, with_alpha(pal.dev_ico, alpha));
        }
        let nx = cx + m.icon + gap + 0.5 * u;
        // GDI+ 的行盒居中比 CSS flex 居中高约 2u、左偏约 0.5u，按设计稿基线补偿。
        draw_num(
            p,
            font,
            font_tight,
            nx,
            2.0 * u,
            m.num_w,
            h,
            cell,
            base,
            threshold,
            pal,
            alpha,
        );
        let lcx = cx + m.icon + gap + m.num_w + gap + m.link_w / 2.0;
        glyph_link(p, cell, mouse, lcx, cy, m.link_w, pal, anim.pulse);
    }
    // 分隔线：1×14，居中。
    let sep_x = x0 + m.cell_w + gap;
    p.fill_rect(sep_x, cy - m.link_w / 2.0, u, m.link_w, pal.card_bd);
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dormant_keeps_last_reading_and_live_colors() {
        for b in [Some(55_u8), Some(10_u8), None] {
            assert_eq!(
                cell_text(CellStatus::Dormant, b),
                cell_text(CellStatus::Live, b),
                "休眠的取字必须与使用中一致"
            );
        }
        assert_eq!(cell_text(CellStatus::Dormant, Some(55)), "55");
        assert_eq!(cell_text(CellStatus::Dormant, Some(10)), "10");
    }

    #[test]
    fn only_absent_uses_dash() {
        assert_eq!(cell_text(CellStatus::Absent, Some(55)), "--");
        assert_eq!(cell_text(CellStatus::Absent, None), "--");
        assert_ne!(
            cell_text(CellStatus::Absent, Some(55)),
            cell_text(CellStatus::Live, Some(55)),
            "只有未连接才显示占位符"
        );
    }

    #[test]
    fn low_battery_is_judged_per_device() {
        let mut snap = Snap::default();
        snap.keyboard = Cell {
            battery: Some(15),
            charge: Charge::Off,
            status: CellStatus::Live,
            wired: false,
        };
        snap.mouse = Cell {
            battery: Some(80),
            charge: Charge::Off,
            status: CellStatus::Live,
            wired: false,
        };
        let low = |c: Cell| c.battery.map_or(false, |b| b <= 20);
        assert!(low(snap.keyboard));
        assert!(!low(snap.mouse), "鼠标电量正常时不能跟着键盘一起变红");
    }

    #[test]
    fn theme_pref_resolves_expected() {
        assert_eq!(ThemePref::Auto.resolve(Theme::Light), Theme::Light);
        assert_eq!(ThemePref::Auto.resolve(Theme::Dark), Theme::Dark);
        assert_eq!(ThemePref::Dark.resolve(Theme::Light), Theme::Dark);
        assert_eq!(ThemePref::Light.resolve(Theme::Dark), Theme::Light);
        assert_eq!(ThemePref::Auto.as_str(), "auto");
        assert_eq!(ThemePref::Dark.as_str(), "dark");
        assert_eq!(ThemePref::Light.as_str(), "light");
    }

    #[test]
    fn dormant_uses_same_link_icon_as_live() {
        let live = Cell {
            battery: Some(50),
            charge: Charge::Off,
            status: CellStatus::Live,
            wired: false,
        };
        let dormant = Cell {
            status: CellStatus::Dormant,
            ..live
        };
        assert_eq!(link_icon(dormant), link_icon(live));
    }

    #[test]
    fn wireless_uses_radio_glyph() {
        let c = Cell {
            battery: Some(50),
            charge: Charge::Off,
            status: CellStatus::Live,
            wired: false,
        };
        assert_eq!(link_icon(c), LinkIcon::Radio);
    }

    #[test]
    fn charging_replaces_link_with_bolt() {
        let c = Cell {
            battery: Some(50),
            charge: Charge::Charging,
            status: CellStatus::Live,
            wired: false,
        };
        assert_eq!(link_icon(c), LinkIcon::Bolt);
        // 充电优先：有线充电也显示雷电，不显示 USB-C。
        assert_eq!(link_icon(Cell { wired: true, ..c }), LinkIcon::Bolt);
    }

    #[test]
    fn low_battery_uses_design_red() {
        let low = Cell {
            battery: Some(15),
            charge: Charge::Off,
            status: CellStatus::Live,
            wired: false,
        };
        let ok = Cell {
            battery: Some(21),
            ..low
        };
        assert_eq!(num_color(low, PAL_DARK.k, 20, &PAL_DARK), PAL_DARK.tray_low);
        assert_eq!(num_color(ok, PAL_DARK.k, 20, &PAL_DARK), PAL_DARK.k);
    }

    #[test]
    fn wired_uses_usb_link_icon() {
        let c = Cell {
            battery: Some(50),
            charge: Charge::Off,
            status: CellStatus::Live,
            wired: true,
        };
        assert_eq!(link_icon(c), LinkIcon::Usb);
    }

    #[test]
    fn tray_icon_handle_is_created_and_destroyed() {
        let mut token: usize = 0;
        let input = sys::GdiplusStartupInput {
            gdiplus_version: 1,
            debug_event_callback: ptr::null_mut(),
            suppress_background_thread: 0,
            suppress_external_codecs: 0,
        };
        unsafe {
            assert_eq!(sys::GdiplusStartup(&mut token, &input, ptr::null_mut()), 0);
        }
        for px in [16, 20, 24] {
            let icon = tray_icon(px).expect("托盘图标应能创建");
            assert_ne!(icon, 0);
            unsafe {
                assert_ne!(sys::DestroyIcon(icon), 0, "DestroyIcon 必须成功");
            }
        }
        unsafe { sys::GdiplusShutdown(token) };
    }
}
