//! 对 C 薄封装的 FFI 绑定。
//!
//! C 侧只负责"把 PTP 命令包成 WPD 命令发出去、把响应取回来"，
//! 所有协议逻辑都在 Rust 侧。为什么这么分工见 `wpd_shim.c` 顶部说明。

#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int, c_void};

/// 不透明的设备句柄
pub enum WpdDeviceRaw {}

unsafe extern "C" {
    // ---- 设备枚举 ----
    fn wpd_list_devices(out_ids: *mut *mut *mut c_char, out_err: *mut *mut c_char) -> c_int;
    fn wpd_free_ids(ids: *mut *mut c_char, count: c_int);
    fn wpd_free_string(s: *mut c_char);

    // ---- 设备会话 ----
    fn wpd_open(pnp_id: *const c_char, out_err: *mut *mut c_char) -> *mut WpdDeviceRaw;
    fn wpd_close(d: *mut WpdDeviceRaw);

    // ---- 命令 ----
    fn wpd_send_command(
        d: *mut WpdDeviceRaw,
        code: u16,
        args: *const u32,
        nargs: u32,
        out_err: *mut *mut c_char,
    ) -> u16;

    fn wpd_send_write_command(
        d: *mut WpdDeviceRaw,
        code: u16,
        args: *const u32,
        nargs: u32,
        data: *const u8,
        datalen: u32,
        out_err: *mut *mut c_char,
    ) -> u16;

    fn wpd_send_read_command(
        d: *mut WpdDeviceRaw,
        code: u16,
        args: *const u32,
        nargs: u32,
        out_data: *mut *mut u8,
        out_len: *mut u32,
        out_err: *mut *mut c_char,
    ) -> u16;

    fn wpd_free_buffer(buf: *mut u8);
}

/// 把 C 侧返回的错误字符串取走并释放
///
/// # Safety
///
/// `err` 必须是 C 侧通过 `malloc` 分配、以 NUL 结尾的字符串，或空指针。
/// 调用后该指针不可再使用（本函数会释放它）。
pub unsafe fn take_error(err: *mut c_char) -> Option<String> {
    if err.is_null() {
        return None;
    }
    let s = std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned();
    wpd_free_string(err);
    Some(s)
}

/// 枚举设备；返回（PnP 标识列表, 错误信息）
pub fn list_devices_raw() -> Result<Vec<String>, String> {
    unsafe {
        let mut ids: *mut *mut c_char = core::ptr::null_mut();
        let mut err: *mut c_char = core::ptr::null_mut();
        let n = wpd_list_devices(&mut ids, &mut err);
        if n < 0 {
            return Err(take_error(err).unwrap_or_else(|| "枚举设备失败".to_string()));
        }
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n as isize {
            let p = *ids.offset(i);
            if p.is_null() {
                continue;
            }
            out.push(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned());
        }
        wpd_free_ids(ids, n);
        Ok(out)
    }
}

/// 打开设备；返回（句柄, 错误信息）
pub fn open_raw(pnp_id: &str) -> Result<*mut WpdDeviceRaw, String> {
    let c_id = std::ffi::CString::new(pnp_id)
        .map_err(|_| "设备标识里含有非法字符（NUL）".to_string())?;
    unsafe {
        let mut err: *mut c_char = core::ptr::null_mut();
        let d = wpd_open(c_id.as_ptr(), &mut err);
        if d.is_null() {
            return Err(take_error(err).unwrap_or_else(|| "打开设备失败".to_string()));
        }
        Ok(d)
    }
}

/// 发送无数据阶段的命令
///
/// # Safety
///
/// `d` 必须是 [`open_raw`] 返回且尚未关闭的设备句柄。
pub unsafe fn send_command_raw(
    d: *mut WpdDeviceRaw,
    code: u16,
    args: &[u32],
) -> Result<u16, String> {
    let mut err: *mut c_char = core::ptr::null_mut();
    let rc = wpd_send_command(d, code, args.as_ptr(), args.len() as u32, &mut err);
    if rc == 0 {
        return Err(take_error(err).unwrap_or_else(|| "命令失败".to_string()));
    }
    Ok(rc)
}

/// 发送带写数据阶段的命令
///
/// # Safety
///
/// `d` 必须是 [`open_raw`] 返回且尚未关闭的设备句柄。
pub unsafe fn send_write_command_raw(
    d: *mut WpdDeviceRaw,
    code: u16,
    args: &[u32],
    data: &[u8],
) -> Result<u16, String> {
    let mut err: *mut c_char = core::ptr::null_mut();
    let rc = wpd_send_write_command(
        d,
        code,
        args.as_ptr(),
        args.len() as u32,
        data.as_ptr(),
        data.len() as u32,
        &mut err,
    );
    if rc == 0 {
        return Err(take_error(err).unwrap_or_else(|| "写命令失败".to_string()));
    }
    Ok(rc)
}

/// 发送带读数据阶段的命令；返回（响应码, 数据）
///
/// # Safety
///
/// `d` 必须是 [`open_raw`] 返回且尚未关闭的设备句柄。
pub unsafe fn send_read_command_raw(
    d: *mut WpdDeviceRaw,
    code: u16,
    args: &[u32],
) -> Result<(u16, Vec<u8>), String> {
    let mut err: *mut c_char = core::ptr::null_mut();
    let mut data: *mut u8 = core::ptr::null_mut();
    let mut len: u32 = 0;
    let rc = wpd_send_read_command(
        d,
        code,
        args.as_ptr(),
        args.len() as u32,
        &mut data,
        &mut len,
        &mut err,
    );
    if rc == 0 {
        if !data.is_null() {
            wpd_free_buffer(data);
        }
        return Err(take_error(err).unwrap_or_else(|| "读命令失败".to_string()));
    }
    let out = if data.is_null() || len == 0 {
        Vec::new()
    } else {
        let v = core::slice::from_raw_parts(data, len as usize).to_vec();
        wpd_free_buffer(data);
        v
    };
    Ok((rc, out))
}

/// 关闭设备句柄
///
/// # Safety
///
/// `d` 必须是 [`open_raw`] 返回且尚未关闭的句柄。调用后不可再使用。
pub unsafe fn close_raw(d: *mut WpdDeviceRaw) {
    if !d.is_null() {
        wpd_close(d);
    }
}

/// 释放 C 侧分配的字符串
///
/// # Safety
///
/// `s` 必须是 C 侧 `malloc` 分配、以 NUL 结尾的字符串，或空指针。
pub unsafe fn free_string(s: *mut c_char) {
    wpd_free_string(s);
}

/// 占位，确保 `c_void` 被引用（某些 cfg 组合下会用到）
pub type _Void = c_void;
