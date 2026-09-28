//! 最小的 COM 绑定：只覆盖 WPD（Windows 便携设备）用到的部分。
//!
//! # 为什么手写
//!
//! `windows-sys` 只提供底层 FFI，不含 COM 包装；完整的 `windows` crate 编译慢、
//! 产物大。我们只需要 4 个接口的十来个方法，手写更小更透明。
//!
//! # 设计：按下标访问 vtable，而不是用结构体字段
//!
//! COM 的 vtable 就是一个函数指针数组。取第 N 个方法，本质是读数组第 N 项。
//!
//! 一开始我按常见做法把 vtable 声明成 `#[repr(C)]` 结构体、用 `(*vtbl).Method`
//! 取函数指针。**在本机实测中这样做取到的值是错的**（打印出来是 `0xffffffff`），
//! 而同一份程序里按下标读 `vtable[3]` 得到的却是正确的函数地址。
//! 所以这里统一改成**按下标读**（也就是 [`slot!`] 宏），这是被实测证实可行的方式。
//!
//! 每个接口都配一组 `const` 下标常量，名字与方法一一对应，
//! 既可读又不会因为"结构体字段顺序"出问题。
//!
//! 下标顺序**逐条对照 Windows SDK 的 IDL/头文件**：
//! - `PortableDeviceApi.h` → `IPortableDeviceManagerVtbl`、`IPortableDeviceVtbl`
//! - `portabledevtypes.idl` → `IPortableDeviceValues`、`IPortableDevicePropVariantCollection`
//! - `unknwn.h` → `IClassFactoryVtbl`

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use core::ffi::c_void;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::PROPERTYKEY;
use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoGetClassObject, CoTaskMemFree};

/// `HRESULT`（成功为 0）
pub type HRESULT = i32;

pub const S_OK: HRESULT = 0;

/// 所有 COM 接口指针的统一表示
pub type IUnknown = *mut c_void;

/// 把 HRESULT 翻译成中文提示
pub fn hresult_message(hr: HRESULT) -> String {
    match hr {
        0 => "成功".to_string(),
        -2147024891 => {
            "拒绝访问（0x80070005）—— 相机可能被其他程序占用（照片应用、资源管理器预览等）"
                .to_string()
        }
        -2147024809 => "参数无效（0x80070057）".to_string(),
        -2147024875 => "找不到设备（0x80070485）—— 相机可能已拔出或关机".to_string(),
        -2147024860 => "设备忙（0x800700E4）".to_string(),
        -2147467259 => "未指定的错误（0x80004005）".to_string(),
        -2147467262 => "该接口不被支持（0x80004002）—— 相机可能不在应用安装模式".to_string(),
        -2147418113 => "灾难性失败（0x8000FFFF）".to_string(),
        _ => format!("错误 0x{:08X}", hr as u32),
    }
}

/// 取 vtable 里第 `$idx` 个方法，并按给定签名调用。
///
/// 用法：`slot!(obj, GetDevices, fn(*mut c_void, *mut *mut u16, *mut u32) -> HRESULT)(args...)`
macro_rules! slot {
    ($obj:expr, $name:literal, $idx:expr, fn($($arg:ty),* $(,)?) $(-> $ret:ty)?) => {{
        let obj: *mut c_void = $obj as *mut c_void;
        // 对象的前 8 字节是 vtable 指针。
        // 用 read_unaligned 逐个把指针读出来，而不是构造 `&Vtbl` 引用 ——
        // 本机实测：用结构体引用取字段会取到错误值，按下标读才可靠。
        let vtbl = core::ptr::read_unaligned(obj as *const *const c_void);
        let f: *const c_void = core::ptr::read(vtbl.add($idx) as *const *const c_void);
        debug_assert!(!f.is_null(), concat!("vtable 槽位 ", $name, " 是空指针"));
        core::mem::transmute::<*const c_void, unsafe extern "system" fn($($arg),*) $(-> $ret)?>(f)
    }};
}

/// `PROPVARIANT` 的变体类型
pub mod vt {
    pub const EMPTY: u16 = 0;
    pub const I4: u16 = 3;
    pub const UI4: u16 = 19;
    pub const UI8: u16 = 21;
    pub const LPWSTR: u16 = 31;
    pub const UNKNOWN: u16 = 13;
    pub const BUFFER: u16 = 65;
}

/// `PROPVARIANT`：x64 上 24 字节（`vt`(2) + 保留(6) + 联合体(16)）
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PROPVARIANT {
    pub vt: u16,
    pub w_reserved1: u16,
    pub w_reserved2: u16,
    pub w_reserved3: u16,
    pub value: u64,
    pub p: *mut c_void,
}

impl Default for PROPVARIANT {
    fn default() -> Self {
        Self {
            vt: vt::EMPTY,
            w_reserved1: 0,
            w_reserved2: 0,
            w_reserved3: 0,
            value: 0,
            p: core::ptr::null_mut(),
        }
    }
}

impl PROPVARIANT {
    pub fn from_u32(v: u32) -> Self {
        Self {
            vt: vt::UI4,
            value: v as u64,
            ..Default::default()
        }
    }
}

/// 从 UTF-16 指针读出字符串（不释放内存）
pub unsafe fn read_wstring(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(core::slice::from_raw_parts(p, len))
}

/// 把 Rust 字符串转成 NUL 结尾的 UTF-16
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ============================================================================
// IUnknown —— 所有接口共有的前三个槽位
// ============================================================================

pub const IUNKNOWN_QUERY_INTERFACE: usize = 0;
pub const IUNKNOWN_ADD_REF: usize = 1;
pub const IUNKNOWN_RELEASE: usize = 2;

/// 释放一个 COM 接口
pub unsafe fn release(this: IUnknown) {
    if this.is_null() {
        return;
    }
    let f = slot!(this, "Release", IUNKNOWN_RELEASE, fn(*mut c_void) -> u32);
    f(this);
}

// ============================================================================
// IClassFactory —— 手工创建 COM 对象
// 对照 unknwn.h：IUnknown(0..2), CreateInstance(3), LockServer(4)
// ============================================================================

pub const ICLASSFACTORY_CREATE_INSTANCE: usize = 3;

/// 通过类工厂创建一个 COM 对象，返回其 `IUnknown` 指针。
///
/// 等价于 `CoCreateInstance`，但每一步都可观测，排查问题时好定位。
pub unsafe fn create_instance(clsid: &GUID, iid: &GUID) -> Result<IUnknown, HRESULT> {
    let mut factory: *mut c_void = core::ptr::null_mut();
    let hr = CoGetClassObject(
        clsid,
        CLSCTX_INPROC_SERVER,
        core::ptr::null_mut(),
        &IID_ICLASS_FACTORY,
        &mut factory,
    );
    if hr != S_OK || factory.is_null() {
        return Err(hr);
    }

    let mut obj: *mut c_void = core::ptr::null_mut();
    // ⚠️ 参数必须与 SDK 一致：`(this, punkOuter, riid, ppv)`
    let f = slot!(
        factory,
        "CreateInstance",
        ICLASSFACTORY_CREATE_INSTANCE,
        fn(*mut c_void, IUnknown, *const GUID, *mut *mut c_void) -> HRESULT
    );
    let hr = f(factory, core::ptr::null_mut(), iid, &mut obj);

    release(factory);

    if hr != S_OK || obj.is_null() {
        return Err(hr);
    }
    Ok(obj)
}

/// `IID_IClassFactory`
const IID_ICLASS_FACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);

// ============================================================================
// IPortableDeviceManager —— 枚举设备
// 对照 PortableDeviceApi.h：IUnknown(0..2), GetDevices(3), RefreshDeviceList(4),
//   GetDeviceFriendlyName(5), GetDeviceDescription(6), GetDeviceManufacturer(7),
//   GetDeviceProperty(8), GetPrivateDevices(9)
// ============================================================================

pub const MANAGER_GET_DEVICES: usize = 3;
pub const MANAGER_GET_DEVICE_FRIENDLY_NAME: usize = 5;

/// 取出所有已连接 WPD 设备的 PnP 标识
pub unsafe fn manager_devices(this: IUnknown) -> Result<Vec<String>, HRESULT> {
    let get_devices = slot!(
        this,
        "GetDevices",
        MANAGER_GET_DEVICES,
        fn(*mut c_void, *mut *mut u16, *mut u32) -> HRESULT
    );

    // 第一次调用只问数量
    let mut count: u32 = 0;
    let hr = get_devices(this, core::ptr::null_mut(), &mut count);
    if hr != S_OK {
        return Err(hr);
    }
    if count == 0 {
        return Ok(Vec::new());
    }

    // 第二次调用填充数组
    let mut ids: Vec<*mut u16> = vec![core::ptr::null_mut(); count as usize];
    let mut got = count;
    let hr = get_devices(this, ids.as_mut_ptr(), &mut got);
    if hr != S_OK {
        return Err(hr);
    }
    ids.truncate(got as usize);

    let mut out = Vec::with_capacity(ids.len());
    for p in ids {
        if p.is_null() {
            continue;
        }
        out.push(read_wstring(p));
        CoTaskMemFree(p as *const c_void);
    }
    Ok(out)
}

/// 设备的显示名（失败返回 None，不算致命）
pub unsafe fn manager_friendly_name(this: IUnknown, id: &[u16]) -> Option<String> {
    let f = slot!(
        this,
        "GetDeviceFriendlyName",
        MANAGER_GET_DEVICE_FRIENDLY_NAME,
        fn(*mut c_void, *const u16, *mut u16, *mut u32) -> HRESULT
    );
    let mut len: u32 = 0;
    let hr = f(this, id.as_ptr(), core::ptr::null_mut(), &mut len);
    if hr != S_OK || len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    let hr = f(this, id.as_ptr(), buf.as_mut_ptr(), &mut len);
    if hr != S_OK {
        return None;
    }
    Some(read_wstring(buf.as_ptr()))
}

// ============================================================================
// IPortableDeviceValues —— 参数包
//
// 对照 portabledevicetypes.idl 里 IPortableDeviceValues 的方法声明顺序。
// 槽位下标逐条数出来，注释里标了方法名，改错一眼能看出来。
// ============================================================================

pub const VALUES_GET_COUNT: usize = 3;
pub const VALUES_GET_AT: usize = 4;
pub const VALUES_SET_VALUE: usize = 5;
pub const VALUES_GET_VALUE: usize = 6;
pub const VALUES_SET_STRING_VALUE: usize = 7;
pub const VALUES_GET_STRING_VALUE: usize = 8;
pub const VALUES_SET_UNSIGNED_INTEGER_VALUE: usize = 9;
pub const VALUES_GET_UNSIGNED_INTEGER_VALUE: usize = 10;
pub const VALUES_SET_SIGNED_INTEGER_VALUE: usize = 11;
pub const VALUES_GET_SIGNED_INTEGER_VALUE: usize = 12;
pub const VALUES_SET_UNSIGNED_LARGE_INTEGER_VALUE: usize = 13;
pub const VALUES_GET_UNSIGNED_LARGE_INTEGER_VALUE: usize = 14;
pub const VALUES_SET_SIGNED_LARGE_INTEGER_VALUE: usize = 15;
pub const VALUES_GET_SIGNED_LARGE_INTEGER_VALUE: usize = 16;
pub const VALUES_SET_FLOAT_VALUE: usize = 17;
pub const VALUES_GET_FLOAT_VALUE: usize = 18;
pub const VALUES_SET_ERROR_VALUE: usize = 19;
pub const VALUES_GET_ERROR_VALUE: usize = 20;
pub const VALUES_SET_KEY_VALUE: usize = 21;
pub const VALUES_GET_KEY_VALUE: usize = 22;
pub const VALUES_SET_BOOL_VALUE: usize = 23;
pub const VALUES_GET_BOOL_VALUE: usize = 24;
pub const VALUES_SET_IUNKNOWN_VALUE: usize = 25;
pub const VALUES_GET_IUNKNOWN_VALUE: usize = 26;
pub const VALUES_SET_GUID_VALUE: usize = 27;
pub const VALUES_GET_GUID_VALUE: usize = 28;
pub const VALUES_SET_BUFFER_VALUE: usize = 29;
pub const VALUES_GET_BUFFER_VALUE: usize = 30;
pub const VALUES_SET_VALUES_VALUE: usize = 31;
pub const VALUES_GET_VALUES_VALUE: usize = 32;
pub const VALUES_SET_PROP_VARIANT_COLLECTION_VALUE: usize = 33;
pub const VALUES_GET_PROP_VARIANT_COLLECTION_VALUE: usize = 34;
pub const VALUES_SET_KEY_COLLECTION_VALUE: usize = 35;
pub const VALUES_GET_KEY_COLLECTION_VALUE: usize = 36;
pub const VALUES_SET_VALUES_COLLECTION_VALUE: usize = 37;
pub const VALUES_GET_VALUES_COLLECTION_VALUE: usize = 38;
pub const VALUES_REMOVE_VALUE: usize = 39;
pub const VALUES_COPY_VALUES_FROM_PROPERTY_STORE: usize = 40;
pub const VALUES_COPY_VALUES_TO_PROPERTY_STORE: usize = 41;
pub const VALUES_CLEAR: usize = 42;

/// `IPortableDeviceValues` 的 vtable 槽位总数（IUnknown 3 + 方法 40）
pub const VALUES_SLOT_COUNT: usize = 43;

pub unsafe fn values_set_guid(
    this: IUnknown,
    key: *const PROPERTYKEY,
    value: *const GUID,
) -> HRESULT {
    let f = slot!(
        this,
        "SetGuidValue",
        VALUES_SET_GUID_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, *const GUID) -> HRESULT
    );
    f(this, key, value)
}

pub unsafe fn values_set_u32(this: IUnknown, key: *const PROPERTYKEY, value: u32) -> HRESULT {
    let f = slot!(
        this,
        "SetUnsignedIntegerValue",
        VALUES_SET_UNSIGNED_INTEGER_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, u32) -> HRESULT
    );
    f(this, key, value)
}

pub unsafe fn values_set_u64(this: IUnknown, key: *const PROPERTYKEY, value: u64) -> HRESULT {
    let f = slot!(
        this,
        "SetUnsignedLargeIntegerValue",
        VALUES_SET_UNSIGNED_LARGE_INTEGER_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, u64) -> HRESULT
    );
    f(this, key, value)
}

pub unsafe fn values_set_string(
    this: IUnknown,
    key: *const PROPERTYKEY,
    value: *const u16,
) -> HRESULT {
    let f = slot!(
        this,
        "SetStringValue",
        VALUES_SET_STRING_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, *const u16) -> HRESULT
    );
    f(this, key, value)
}

pub unsafe fn values_set_buffer(
    this: IUnknown,
    key: *const PROPERTYKEY,
    data: *const u8,
    len: u32,
) -> HRESULT {
    let f = slot!(
        this,
        "SetBufferValue",
        VALUES_SET_BUFFER_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, *const u8, u32) -> HRESULT
    );
    f(this, key, data, len)
}

pub unsafe fn values_set_collection(
    this: IUnknown,
    key: *const PROPERTYKEY,
    value: IUnknown,
) -> HRESULT {
    let f = slot!(
        this,
        "SetIPortableDevicePropVariantCollectionValue",
        VALUES_SET_PROP_VARIANT_COLLECTION_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, IUnknown) -> HRESULT
    );
    f(this, key, value)
}

pub unsafe fn values_set_values(
    this: IUnknown,
    key: *const PROPERTYKEY,
    value: IUnknown,
) -> HRESULT {
    let f = slot!(
        this,
        "SetIPortableDeviceValuesValue",
        VALUES_SET_VALUES_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, IUnknown) -> HRESULT
    );
    f(this, key, value)
}

pub unsafe fn values_get_u32(this: IUnknown, key: *const PROPERTYKEY) -> Result<u32, HRESULT> {
    let f = slot!(
        this,
        "GetUnsignedIntegerValue",
        VALUES_GET_UNSIGNED_INTEGER_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, *mut u32) -> HRESULT
    );
    let mut v: u32 = 0;
    let hr = f(this, key, &mut v);
    if hr == S_OK { Ok(v) } else { Err(hr) }
}

pub unsafe fn values_get_u64(this: IUnknown, key: *const PROPERTYKEY) -> Result<u64, HRESULT> {
    let f = slot!(
        this,
        "GetUnsignedLargeIntegerValue",
        VALUES_GET_UNSIGNED_LARGE_INTEGER_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, *mut u64) -> HRESULT
    );
    let mut v: u64 = 0;
    let hr = f(this, key, &mut v);
    if hr == S_OK { Ok(v) } else { Err(hr) }
}

/// 取 HRESULT 类型的值（WPD 把命令的内部错误放在这里）
pub unsafe fn values_get_error(
    this: IUnknown,
    key: *const PROPERTYKEY,
) -> Result<HRESULT, HRESULT> {
    let f = slot!(
        this,
        "GetErrorValue",
        VALUES_GET_ERROR_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, *mut HRESULT) -> HRESULT
    );
    let mut v: HRESULT = 0;
    let hr = f(this, key, &mut v);
    if hr == S_OK { Ok(v) } else { Err(hr) }
}

pub unsafe fn values_get_string(this: IUnknown, key: *const PROPERTYKEY) -> Result<String, HRESULT> {
    let f = slot!(
        this,
        "GetStringValue",
        VALUES_GET_STRING_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, *mut *mut u16) -> HRESULT
    );
    let mut p: *mut u16 = core::ptr::null_mut();
    let hr = f(this, key, &mut p);
    if hr != S_OK || p.is_null() {
        return Err(hr);
    }
    let s = read_wstring(p);
    CoTaskMemFree(p as *const c_void);
    Ok(s)
}

/// 取二进制数据（拷贝一份并释放 COM 缓冲）
pub unsafe fn values_get_buffer(
    this: IUnknown,
    key: *const PROPERTYKEY,
) -> Result<Vec<u8>, HRESULT> {
    let f = slot!(
        this,
        "GetBufferValue",
        VALUES_GET_BUFFER_VALUE,
        fn(*mut c_void, *const PROPERTYKEY, *mut *mut u8, *mut u32) -> HRESULT
    );
    let mut p: *mut u8 = core::ptr::null_mut();
    let mut len: u32 = 0;
    let hr = f(this, key, &mut p, &mut len);
    if hr != S_OK {
        return Err(hr);
    }
    if p.is_null() {
        return Ok(Vec::new());
    }
    let data = core::slice::from_raw_parts(p, len as usize).to_vec();
    CoTaskMemFree(p as *const c_void);
    Ok(data)
}

// ============================================================================
// IPortableDevicePropVariantCollection —— 参数数组
// 对照 IDL：IUnknown(0..2), GetCount(3), GetAt(4), Add(5), GetType(6),
//   ChangeType(7), Clear(8), RemoveAt(9)
// ============================================================================

pub const COLLECTION_ADD: usize = 5;

pub unsafe fn collection_add_u32(this: IUnknown, value: u32) -> HRESULT {
    let f = slot!(
        this,
        "Add",
        COLLECTION_ADD,
        fn(*mut c_void, *const PROPVARIANT) -> HRESULT
    );
    let v = PROPVARIANT::from_u32(value);
    f(this, &v)
}

// ============================================================================
// IPortableDevice —— 打开设备、发送命令
// 对照 PortableDeviceApi.h：IUnknown(0..2), Open(3), SendCommand(4), Content(5),
//   Capabilities(6), Cancel(7), Close(8), Advise(9), Unadvise(10), GetPnPDeviceID(11)
// ============================================================================

pub const DEVICE_OPEN: usize = 3;
pub const DEVICE_SEND_COMMAND: usize = 4;

pub unsafe fn device_open(this: IUnknown, id: *const u16, client_info: IUnknown) -> HRESULT {
    let f = slot!(
        this,
        "Open",
        DEVICE_OPEN,
        fn(*mut c_void, *const u16, IUnknown) -> HRESULT
    );
    f(this, id, client_info)
}

/// 发送 WPD 命令，返回结果参数包（调用方负责 release）
pub unsafe fn device_send_command(
    this: IUnknown,
    params: IUnknown,
) -> Result<IUnknown, HRESULT> {
    let f = slot!(
        this,
        "SendCommand",
        DEVICE_SEND_COMMAND,
        fn(*mut c_void, u32, IUnknown, *mut IUnknown) -> HRESULT
    );
    let mut results: IUnknown = core::ptr::null_mut();
    let hr = f(this, 0, params, &mut results);
    if hr != S_OK {
        return Err(hr);
    }
    if results.is_null() {
        return Err(-2147467259);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_string_helpers() {
        let w = to_wide("A6300");
        assert_eq!(w.len(), 6);
        assert_eq!(*w.last().unwrap(), 0);
        unsafe {
            assert_eq!(read_wstring(w.as_ptr()), "A6300");
            assert_eq!(read_wstring(core::ptr::null()), "");
        }
    }

    #[test]
    fn propvariant_layout_is_24_bytes() {
        assert_eq!(core::mem::size_of::<PROPVARIANT>(), 24);
        assert_eq!(core::mem::align_of::<PROPVARIANT>(), 8);
    }

    #[test]
    fn propertykey_layout() {
        // PROPERTYKEY = GUID(16) + DWORD(4)，按 4 字节对齐 → 20 字节
        assert_eq!(core::mem::size_of::<PROPERTYKEY>(), 20);
    }

    /// 槽位下标必须单调递增且不重复（防止抄错或漏数）
    #[test]
    fn slot_indices_are_sequential() {
        let values_indices = [
            VALUES_GET_COUNT,
            VALUES_GET_AT,
            VALUES_SET_VALUE,
            VALUES_GET_VALUE,
            VALUES_SET_STRING_VALUE,
            VALUES_GET_STRING_VALUE,
            VALUES_SET_UNSIGNED_INTEGER_VALUE,
            VALUES_GET_UNSIGNED_INTEGER_VALUE,
            VALUES_SET_SIGNED_INTEGER_VALUE,
            VALUES_GET_SIGNED_INTEGER_VALUE,
            VALUES_SET_UNSIGNED_LARGE_INTEGER_VALUE,
            VALUES_GET_UNSIGNED_LARGE_INTEGER_VALUE,
            VALUES_SET_SIGNED_LARGE_INTEGER_VALUE,
            VALUES_GET_SIGNED_LARGE_INTEGER_VALUE,
            VALUES_SET_FLOAT_VALUE,
            VALUES_GET_FLOAT_VALUE,
            VALUES_SET_ERROR_VALUE,
            VALUES_GET_ERROR_VALUE,
            VALUES_SET_KEY_VALUE,
            VALUES_GET_KEY_VALUE,
            VALUES_SET_BOOL_VALUE,
            VALUES_GET_BOOL_VALUE,
            VALUES_SET_IUNKNOWN_VALUE,
            VALUES_GET_IUNKNOWN_VALUE,
            VALUES_SET_GUID_VALUE,
            VALUES_GET_GUID_VALUE,
            VALUES_SET_BUFFER_VALUE,
            VALUES_GET_BUFFER_VALUE,
            VALUES_SET_VALUES_VALUE,
            VALUES_GET_VALUES_VALUE,
            VALUES_SET_PROP_VARIANT_COLLECTION_VALUE,
            VALUES_GET_PROP_VARIANT_COLLECTION_VALUE,
            VALUES_SET_KEY_COLLECTION_VALUE,
            VALUES_GET_KEY_COLLECTION_VALUE,
            VALUES_SET_VALUES_COLLECTION_VALUE,
            VALUES_GET_VALUES_COLLECTION_VALUE,
            VALUES_REMOVE_VALUE,
            VALUES_COPY_VALUES_FROM_PROPERTY_STORE,
            VALUES_COPY_VALUES_TO_PROPERTY_STORE,
            VALUES_CLEAR,
        ];
        assert_eq!(values_indices.len(), VALUES_SLOT_COUNT - 3, "方法数量应为 40");
        for (i, idx) in values_indices.iter().enumerate() {
            assert_eq!(*idx, 3 + i, "第 {i} 个方法的下标应为 {}", 3 + i);
        }
    }
}
