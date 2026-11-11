//! 雷柏私有 HID 帧解析（纯函数，无 IO，可单元测试）。
//!
//! 只读被动接收设备主动广播的输入报告，不做任何查询/写入。
//!
//! 规格依据（本机实测）：
//! - 鼠标 VT7 PRO V2 2.4G（VID 0x24AE / PID 0x14A7）：Report 0x07，19 字节，
//!   电量 `buf[8]`，状态 `buf[7]`（bit1=充电中）。
//! - 键盘 V700DIY-98 2.4G（VID 0x24AE / PID 0x1924）：Report 0xBD，24 字节，
//!   电量 `buf[8]`，`buf[7]`：1=放电、2=充电、3=已满，其它视为无效。

/// 充电状态。`Unknown` 表示该帧的充电位无效，应保留上一次有效值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charge {
    Off,
    Charging,
    Full,
    #[default]
    Unknown,
}

impl Charge {
    /// 是否插着电：`Charging`（充电中）与 `Full`（已充满）都属于插电状态，
    /// 只有 `Off`/`Unknown` 才算未插电（拔电）。
    pub fn plugged(self) -> bool {
        matches!(self, Charge::Charging | Charge::Full)
    }
}

/// 一帧有效读数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub battery: u8,
    pub charge: Charge,
}

/// 鼠标帧长（InputReportByteLength，含 Report ID）。
pub const MOUSE_FRAME_LEN: usize = 19;
/// 键盘帧长（本机实测；解析时按 >=9 宽松处理，避免固件版本差异导致整帧丢弃）。
#[allow(dead_code)]
pub const KEYBOARD_FRAME_LEN: usize = 24;

pub const MOUSE_REPORT_ID: u8 = 0x07;
pub const KEYBOARD_REPORT_ID: u8 = 0xBD;

/// 解析鼠标 0x07 帧。无效帧返回 `None`（调用方保留上次数值）。
pub fn parse_mouse(buf: &[u8]) -> Option<Reading> {
    if buf.len() != MOUSE_FRAME_LEN || buf[0] != MOUSE_REPORT_ID {
        return None;
    }
    let battery = buf[8];
    if battery > 100 {
        return None;
    }
    let charge = match buf[7] {
        // 实测切换瞬间会出现几帧 0x00，必须保留上一次有效值。
        0x00 => Charge::Unknown,
        0x02 => Charge::Charging,
        0x03 => Charge::Full,
        s if s & 0x02 != 0 => Charge::Charging,
        _ => Charge::Off,
    };
    Some(Reading { battery, charge })
}

/// 解析键盘 0xBD 帧。无效帧返回 `None`。
pub fn parse_keyboard(buf: &[u8]) -> Option<Reading> {
    if buf.len() < 9 || buf[0] != KEYBOARD_REPORT_ID {
        return None;
    }
    let battery = buf[8];
    if battery > 100 {
        return None;
    }
    let charge = match buf[7] {
        1 => Charge::Off,
        2 => Charge::Charging,
        3 => Charge::Full,
        _ => Charge::Unknown,
    };
    Some(Reading { battery, charge })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUSE_IDLE: &[u8] = &[
        0x07, 0x40, 0x02, 0xB0, 0x04, 0xB0, 0x04, 0x01, 0x3C, 0x00, 0x04, 0x02, 0x00, 0xA7, 0x46,
        0x10, 0x02, 0x02, 0xBD,
    ];

    fn mouse_with(status: u8, battery: u8, len: usize) -> Vec<u8> {
        let mut v = MOUSE_IDLE.to_vec();
        v[7] = status;
        v[8] = battery;
        v.resize(len, 0);
        v
    }

    fn kb(status: u8, battery: u8) -> Vec<u8> {
        let mut v = vec![0u8; KEYBOARD_FRAME_LEN];
        v[0] = 0xBD;
        v[1] = 0x01;
        v[3] = 0x68;
        v[4] = 0x4C;
        v[5] = 0x64;
        v[7] = status;
        v[8] = battery;
        v
    }

    #[test]
    fn mouse_real_frame_60_off() {
        let r = parse_mouse(MOUSE_IDLE).unwrap();
        assert_eq!(r.battery, 60);
        assert_eq!(r.charge, Charge::Off);
    }

    #[test]
    fn mouse_charging_bit() {
        assert_eq!(
            parse_mouse(&mouse_with(0x02, 66, 19)).unwrap().charge,
            Charge::Charging
        );
        assert_eq!(
            parse_mouse(&mouse_with(0x03, 66, 19)).unwrap().charge,
            Charge::Full
        );
    }

    #[test]
    fn mouse_invalid_status_keeps_unknown() {
        assert_eq!(
            parse_mouse(&mouse_with(0x00, 60, 19)).unwrap().charge,
            Charge::Unknown
        );
    }

    #[test]
    fn mouse_borders() {
        assert_eq!(parse_mouse(&mouse_with(0x01, 0, 19)).unwrap().battery, 0);
        assert_eq!(
            parse_mouse(&mouse_with(0x01, 100, 19)).unwrap().battery,
            100
        );
        assert_eq!(parse_mouse(&mouse_with(0x01, 20, 19)).unwrap().battery, 20);
        assert_eq!(parse_mouse(&mouse_with(0x01, 21, 19)).unwrap().battery, 21);
    }

    #[test]
    fn mouse_rejects_bad_frames() {
        // 电量越界
        assert!(parse_mouse(&mouse_with(0x01, 101, 19)).is_none());
        assert!(parse_mouse(&mouse_with(0x01, 0xFF, 19)).is_none());
        // 帧长不符（短帧/长帧）
        assert!(parse_mouse(&mouse_with(0x01, 60, 18)).is_none());
        assert!(parse_mouse(&mouse_with(0x01, 60, 20)).is_none());
        // Report ID 不符
        let mut v = MOUSE_IDLE.to_vec();
        v[0] = 0xBD;
        assert!(parse_mouse(&v).is_none());
        // 空帧
        assert!(parse_mouse(&[]).is_none());
    }

    #[test]
    fn keyboard_real_frame_29_off() {
        let r = parse_keyboard(&kb(1, 29)).unwrap();
        assert_eq!(r.battery, 29);
        assert_eq!(r.charge, Charge::Off);
    }

    #[test]
    fn keyboard_charge_states() {
        assert_eq!(parse_keyboard(&kb(2, 31)).unwrap().charge, Charge::Charging);
        assert_eq!(parse_keyboard(&kb(3, 100)).unwrap().charge, Charge::Full);
        assert_eq!(parse_keyboard(&kb(0, 29)).unwrap().charge, Charge::Unknown);
        assert_eq!(parse_keyboard(&kb(4, 29)).unwrap().charge, Charge::Unknown);
    }

    #[test]
    fn keyboard_rejects_bad_frames() {
        assert!(parse_keyboard(&kb(1, 101)).is_none());
        assert!(parse_keyboard(&[0xBD]).is_none());
        let mut v = kb(1, 29);
        v[0] = 0x07;
        assert!(parse_keyboard(&v).is_none());
    }

    #[test]
    fn keyboard_accepts_longer_frame() {
        // 固件若发 32 字节，按前 9 字节语义仍可解析。
        let mut v = kb(2, 30);
        v.resize(32, 0);
        assert_eq!(parse_keyboard(&v).unwrap().battery, 30);
    }
}
