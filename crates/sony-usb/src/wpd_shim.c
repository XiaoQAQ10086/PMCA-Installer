/*
 * WPD（Windows 便携设备）薄封装。
 *
 * 为什么用 C 而不用纯 Rust 手写 COM：
 *
 * 相机在 Windows 上被 MTP 驱动独占，只能通过 WPD 的 COM 接口说话。
 * 我先用纯 Rust 手写 COM 绑定，运行时读到的 vtable 指针比真实表**靠前一字节**，
 * 导致按下标取函数指针全部错位、一调用就访问冲突（0xC0000005）。
 * 改用 C 之后一切正常 —— 说明问题在"Rust 里解析 COM vtable"这一环，
 * 而不是 WPD 本身。
 *
 * 另一个理由：原项目的 Python 版用的 comtypes 也是 C 层实现，
 * 也就是说**这条路已经被原项目验证过可行**，比自创一套绑定可靠。
 *
 * 本文件只做一件事：把 PTP 命令包成 WPD 命令发出去、把响应取回来。
 * 所有协议逻辑都在 Rust 侧。
 *
 * 对应原项目 pmca/usb/driver/windows/wpd.py。
 */

#define WIN32_LEAN_AND_MEAN
/*
 * ⚠️ 关键：WPD 的接口在头文件里是 **C 风格** —— 结构体里只有 `lpVtbl` 一个成员，
 * 没有 C++ 的虚函数成员。必须定义 CINTERFACE + COBJMACROS，
 * 头文件才会把 `obj->Method(...)` 展开成 `obj->lpVtbl->Method(obj, ...)`。
 *
 * 不定义的话，编译器只看到 `lpVtbl` 成员，报「Method 不是 XXX 的成员」。
 * 这也是我在 Rust 侧手写绑定时踩的坑：**接口是 C 风格，不是 C++ 风格**。
 */
#define CINTERFACE
#define COBJMACROS
#include <windows.h>
#include <objbase.h>
#include <PortableDeviceTypes.h>
#include <PortableDeviceApi.h>
#include <PortableDevice.h>
#include <WpdMtpExtensions.h>
#include <propvarutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/*
 * ⚠️ `PortableDeviceApi.h` 里对 `IPortableDeviceValues` 等接口
 * **只有前向声明**，完整定义在 `PortableDeviceTypes.h`。
 * 漏掉后者的话，编译器看到的是不完整类型，
 * `params->lpVtbl->SetGuidValue(params, ...)` 会报
 * "不是 IPortableDeviceValues 的成员"。
 *
 * 这也是选用 C 做这一层的原因：手写绑定时 vtable 布局本身没错，
 * 只是参照的接口定义不完整，而这类错误在 C 里编译器会直接指出来。
 */

/* ------------------------------------------------------------------ */
/* 内存辅助                                                            */
/* ------------------------------------------------------------------ */

static void *xmalloc(size_t n) {
    void *p = malloc(n ? n : 1);
    return p;
}

static void xfree(void *p) {
    if (p) free(p);
}

/* 宽字符串 → 新建的 UTF-8 字符串（调用方负责 free） */
static char *wide_to_utf8(const wchar_t *w) {
    if (!w) return NULL;
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
    if (n <= 0) return NULL;
    char *out = (char *)xmalloc((size_t)n);
    if (!out) return NULL;
    WideCharToMultiByte(CP_UTF8, 0, w, -1, out, n, NULL, NULL);
    return out;
}

/* UTF-8 → 新建的宽字符串（调用方负责 free） */
static wchar_t *utf8_to_wide(const char *s) {
    if (!s) return NULL;
    int n = MultiByteToWideChar(CP_UTF8, 0, s, -1, NULL, 0);
    if (n <= 0) return NULL;
    wchar_t *out = (wchar_t *)xmalloc((size_t)n * sizeof(wchar_t));
    if (!out) return NULL;
    MultiByteToWideChar(CP_UTF8, 0, s, -1, out, n);
    return out;
}

/* ------------------------------------------------------------------ */
/* 错误信息                                                            */
/* ------------------------------------------------------------------ */

/* 把 HRESULT 写成一句话（调用方负责 free）。缓冲区大小 256 足够。 */
static char *describe_hresult(HRESULT hr) {
    char *buf = (char *)xmalloc(256);
    if (!buf) return NULL;
    switch (hr) {
        case S_OK:
            strcpy(buf, "成功");
            break;
        case E_ACCESSDENIED:
            strcpy(buf, "拒绝访问（相机可能被其他程序占用）");
            break;
        case E_INVALIDARG:
            strcpy(buf, "参数无效");
            break;
        case E_OUTOFMEMORY:
            strcpy(buf, "内存不足");
            break;
        case E_NOTIMPL:
            strcpy(buf, "该功能未实现");
            break;
        case E_FAIL:
            strcpy(buf, "未指定的错误");
            break;
        case (HRESULT)0x80070002:
            strcpy(buf, "找不到设备（相机可能已拔出或关机）");
            break;
        case (HRESULT)0x800700AA:
            strcpy(buf, "设备忙");
            break;
        default: {
            wchar_t wbuf[256];
            DWORD n = FormatMessageW(
                FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                NULL, (DWORD)hr, 0, wbuf, 256, NULL);
            /* 去掉结尾换行 */
            while (n > 0 & (wbuf[n - 1] == L'\r' || wbuf[n - 1] == L'\n')) {
                wbuf[--n] = L'\0';
            }
            if (n > 0) {
                char *utf8 = wide_to_utf8(wbuf);
                if (utf8) {
                    snprintf(buf, 256, "0x%08X: %s", (unsigned)hr, utf8);
                    xfree(utf8);
                } else {
                    snprintf(buf, 256, "0x%08X", (unsigned)hr);
                }
            } else {
                snprintf(buf, 256, "0x%08X", (unsigned)hr);
            }
            break;
        }
    }
    return buf;
}

/* ------------------------------------------------------------------ */
/* 设备枚举                                                            */
/* ------------------------------------------------------------------ */

/*
 * 枚举所有 WPD 设备的 PnP 标识。
 *
 * out_ids：接收一个字符串数组的指针（每项 UTF-8，需用 wpd_free_ids 释放）
 * 返回设备数量；负数表示失败（错误信息见 *out_err）
 */
int wpd_list_devices(char ***out_ids, char **out_err) {
    *out_ids = NULL;
    if (out_err) *out_err = NULL;

    IPortableDeviceManager *mgr = NULL;
    HRESULT hr = CoCreateInstance(&CLSID_PortableDeviceManager, NULL,
                                  CLSCTX_INPROC_SERVER,
                                  &IID_IPortableDeviceManager, (void **)&mgr);
    if (FAILED(hr) || !mgr) {
        if (out_err) *out_err = describe_hresult(hr);
        return -1;
    }

    /* 第一次：问数量 */
    DWORD count = 0;
    hr = mgr->lpVtbl->GetDevices(mgr, NULL, &count);
    if (FAILED(hr)) {
        if (out_err) *out_err = describe_hresult(hr);
        mgr->lpVtbl->Release(mgr);
        return -1;
    }
    if (count == 0) {
        mgr->lpVtbl->Release(mgr);
        return 0;
    }

    /* 第二次：填充数组 */
    LPWSTR *ids = (LPWSTR *)xmalloc(sizeof(LPWSTR) * count);
    if (!ids) {
        if (out_err) *out_err = _strdup("内存不足");
        mgr->lpVtbl->Release(mgr);
        return -1;
    }
    DWORD got = count;
    hr = mgr->lpVtbl->GetDevices(mgr, ids, &got);
    if (FAILED(hr)) {
        if (out_err) *out_err = describe_hresult(hr);
        xfree(ids);
        mgr->lpVtbl->Release(mgr);
        return -1;
    }

    char **result = (char **)xmalloc(sizeof(char *) * (got ? got : 1));
    if (!result) {
        xfree(ids);
        if (out_err) *out_err = _strdup("内存不足");
        mgr->lpVtbl->Release(mgr);
        return -1;
    }
    for (DWORD i = 0; i < got; i++) {
        result[i] = wide_to_utf8(ids[i]);
        CoTaskMemFree(ids[i]);
    }
    xfree(ids);
    mgr->lpVtbl->Release(mgr);

    *out_ids = result;
    return (int)got;
}

void wpd_free_ids(char **ids, int count) {
    if (!ids) return;
    for (int i = 0; i < count; i++) xfree(ids[i]);
    xfree(ids);
}

void wpd_free_string(char *s) {
    xfree(s);
}

/* ------------------------------------------------------------------ */
/* 设备会话                                                            */
/* ------------------------------------------------------------------ */

struct wpd_device {
    IPortableDevice *device;
};

/*
 * 打开指定 PnP 标识的设备。
 * 成功返回句柄；失败返回 NULL 并填 *out_err。
 */
struct wpd_device *wpd_open(const char *pnp_id, char **out_err) {
    if (out_err) *out_err = NULL;

    IPortableDevice *device = NULL;
    HRESULT hr = CoCreateInstance(&CLSID_PortableDevice, NULL,
                                  CLSCTX_INPROC_SERVER, &IID_IPortableDevice,
                                  (void **)&device);
    if (FAILED(hr) || !device) {
        if (out_err) *out_err = describe_hresult(hr);
        return NULL;
    }

    /* 客户端信息：一个空的参数包 */
    IPortableDeviceValues *client_info = NULL;
    hr = CoCreateInstance(&CLSID_PortableDeviceValues, NULL,
                          CLSCTX_INPROC_SERVER, &IID_IPortableDeviceValues,
                          (void **)&client_info);
    if (FAILED(hr) || !client_info) {
        if (out_err) *out_err = describe_hresult(hr);
        device->lpVtbl->Release(device);
        return NULL;
    }

    wchar_t *wid = utf8_to_wide(pnp_id);
    hr = device->lpVtbl->Open(device, wid, client_info);
    xfree(wid);
    client_info->lpVtbl->Release(client_info);

    if (FAILED(hr)) {
        if (out_err) *out_err = describe_hresult(hr);
        device->lpVtbl->Release(device);
        return NULL;
    }

    struct wpd_device *d = (struct wpd_device *)xmalloc(sizeof(struct wpd_device));
    if (!d) {
        if (out_err) *out_err = _strdup("内存不足");
        device->lpVtbl->Release(device);
        return NULL;
    }
    d->device = device;
    return d;
}

void wpd_close(struct wpd_device *d) {
    if (!d) return;
    if (d->device) d->device->lpVtbl->Release(d->device);
    xfree(d);
}

/* 组装 "命令 + 操作码 + 参数" 的参数包 */
static IPortableDeviceValues *build_command(REFPROPERTYKEY command, WORD code,
                                             const DWORD *args, DWORD nargs) {
    IPortableDeviceValues *params = NULL;
    HRESULT hr = CoCreateInstance(&CLSID_PortableDeviceValues, NULL,
                                  CLSCTX_INPROC_SERVER,
                                  &IID_IPortableDeviceValues, (void **)&params);
    if (FAILED(hr) || !params) return NULL;

    /* `command` 已经是 REFPROPERTYKEY（等价于 `const PROPERTYKEY *`），
       所以成员访问用 `->`，传给 SetGuidValue 时用 `&command->fmtid` */
    params->lpVtbl->SetGuidValue(params, &WPD_PROPERTY_COMMON_COMMAND_CATEGORY,
                                 &command->fmtid);
    params->lpVtbl->SetUnsignedIntegerValue(params,
                                            &WPD_PROPERTY_COMMON_COMMAND_ID,
                                            command->pid);
    params->lpVtbl->SetUnsignedIntegerValue(
        params, &WPD_PROPERTY_MTP_EXT_OPERATION_CODE, code);

    IPortableDevicePropVariantCollection *coll = NULL;
    hr = CoCreateInstance(&CLSID_PortableDevicePropVariantCollection, NULL,
                          CLSCTX_INPROC_SERVER,
                          &IID_IPortableDevicePropVariantCollection, (void **)&coll);
    if (FAILED(hr) || !coll) {
        params->lpVtbl->Release(params);
        return NULL;
    }
    for (DWORD i = 0; i < nargs; i++) {
        PROPVARIANT pv;
        PropVariantInit(&pv);
        pv.vt = VT_UI4;
        pv.ulVal = args[i];
        coll->lpVtbl->Add(coll, &pv);
        PropVariantClear(&pv);
    }
    params->lpVtbl->SetIPortableDevicePropVariantCollectionValue(params, 
        &WPD_PROPERTY_MTP_EXT_OPERATION_PARAMS, coll);
    coll->lpVtbl->Release(coll);
    return params;
}

/* 发送并检查 WPD 层错误；成功时返回结果包（调用方负责 Release） */
static IPortableDeviceValues *send_command(IPortableDevice *device,
                                           IPortableDeviceValues *params,
                                           char **out_err) {
    IPortableDeviceValues *results = NULL;
    HRESULT hr = device->lpVtbl->SendCommand(device, 0, params, &results);
    if (FAILED(hr) || !results) {
        if (out_err) *out_err = describe_hresult(hr);
        return NULL;
    }
    /* WPD 会把命令本身的错误放在 HRESULT 属性里，即使 SendCommand 成功 */
    HRESULT inner = S_OK;
    if (SUCCEEDED(results->lpVtbl->GetErrorValue(results, &WPD_PROPERTY_COMMON_HRESULT, &inner)) &
        FAILED(inner)) {
        if (out_err) *out_err = describe_hresult(inner);
        results->lpVtbl->Release(results);
        return NULL;
    }
    return results;
}

/*
 * 发送 PTP 命令（无数据阶段）。
 * 返回 MTP 响应码（如 0x2001 = OK）；0 表示失败（错误见 *out_err）。
 */
WORD wpd_send_command(struct wpd_device *d, WORD code, const DWORD *args,
                      DWORD nargs, char **out_err) {
    if (out_err) *out_err = NULL;
    IPortableDeviceValues *params =
        build_command(&WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITHOUT_DATA_PHASE,
                      code, args, nargs);
    if (!params) {
        if (out_err) *out_err = _strdup("创建参数包失败");
        return 0;
    }
    IPortableDeviceValues *results = send_command(d->device, params, out_err);
    params->lpVtbl->Release(params);
    if (!results) return 0;

    DWORD rc = 0;
    results->lpVtbl->GetUnsignedIntegerValue(results, &WPD_PROPERTY_MTP_EXT_RESPONSE_CODE, &rc);
    results->lpVtbl->Release(results);
    return (WORD)rc;
}

/*
 * 发送 PTP 命令（写数据阶段）。
 *
 * ⚠️ data 指针只在本次调用期间有效。
 */
WORD wpd_send_write_command(struct wpd_device *d, WORD code, const DWORD *args,
                            DWORD nargs, const BYTE *data, DWORD datalen,
                            char **out_err) {
    if (out_err) *out_err = NULL;

    /* 第一步：告诉相机要写多少 */
    IPortableDeviceValues *params =
        build_command(&WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_WRITE,
                      code, args, nargs);
    if (!params) {
        if (out_err) *out_err = _strdup("创建参数包失败");
        return 0;
    }
    params->lpVtbl->SetUnsignedLargeIntegerValue(params, 
        &WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE, datalen);

    IPortableDeviceValues *first = send_command(d->device, params, out_err);
    params->lpVtbl->Release(params);
    if (!first) return 0;

    LPWSTR context = NULL;
    HRESULT hr = first->lpVtbl->GetStringValue(first, &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT,
                                       &context);
    first->lpVtbl->Release(first);
    if (FAILED(hr) || !context) {
        if (out_err) *out_err = _strdup("取传输上下文失败");
        if (context) CoTaskMemFree(context);
        return 0;
    }

    /* 第二步：写数据 */
    IPortableDeviceValues *cmd = NULL;
    hr = CoCreateInstance(&CLSID_PortableDeviceValues, NULL, CLSCTX_INPROC_SERVER,
                          &IID_IPortableDeviceValues, (void **)&cmd);
    if (FAILED(hr) || !cmd) {
        if (out_err) *out_err = describe_hresult(hr);
        CoTaskMemFree(context);
        return 0;
    }
    cmd->lpVtbl->SetGuidValue(cmd, &WPD_PROPERTY_COMMON_COMMAND_CATEGORY,
                      &WPD_COMMAND_MTP_EXT_WRITE_DATA.fmtid);
    cmd->lpVtbl->SetUnsignedIntegerValue(cmd, &WPD_PROPERTY_COMMON_COMMAND_ID,
                                 WPD_COMMAND_MTP_EXT_WRITE_DATA.pid);
    cmd->lpVtbl->SetStringValue(cmd, &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT, context);
    cmd->lpVtbl->SetUnsignedLargeIntegerValue(cmd, 
        &WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_TO_WRITE, datalen);
    cmd->lpVtbl->SetBufferValue(cmd, &WPD_PROPERTY_MTP_EXT_TRANSFER_DATA, data, datalen);
    IPortableDeviceValues *w = send_command(d->device, cmd, out_err);
    cmd->lpVtbl->Release(cmd);
    if (!w) {
        CoTaskMemFree(context);
        return 0;
    }
    w->lpVtbl->Release(w);

    /* 第三步：结束传输并取响应码 */
    IPortableDeviceValues *end = NULL;
    hr = CoCreateInstance(&CLSID_PortableDeviceValues, NULL, CLSCTX_INPROC_SERVER,
                          &IID_IPortableDeviceValues, (void **)&end);
    if (FAILED(hr) || !end) {
        if (out_err) *out_err = describe_hresult(hr);
        CoTaskMemFree(context);
        return 0;
    }
    end->lpVtbl->SetGuidValue(end, &WPD_PROPERTY_COMMON_COMMAND_CATEGORY,
                      &WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER.fmtid);
    end->lpVtbl->SetUnsignedIntegerValue(end, &WPD_PROPERTY_COMMON_COMMAND_ID,
                                 WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER.pid);
    end->lpVtbl->SetStringValue(end, &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT, context);
    CoTaskMemFree(context);

    IPortableDeviceValues *e = send_command(d->device, end, out_err);
    end->lpVtbl->Release(end);
    if (!e) return 0;

    DWORD rc = 0;
    e->lpVtbl->GetUnsignedIntegerValue(e, &WPD_PROPERTY_MTP_EXT_RESPONSE_CODE, &rc);
    e->lpVtbl->Release(e);
    return (WORD)rc;
}

/*
 * 发送 PTP 命令（读数据阶段）。
 *
 * ⚠️⚠️ 这里有两个非常容易踩的坑，都是**真机调试**时发现的：
 *
 * 1. **第一步返回的响应码恒为 0x0000，那不是错误！**
 *    MTP 的响应码要等**整个数据传输结束**（第三步）之后才有意义。
 *    如果在第一步就检查响应码，会把它误判成失败。
 *
 * 2. **`END_DATA_TRANSFER` 不是"可选的收尾"，而是协议必需的一步。**
 *    漏掉它，这次传输就没有正常结束，相机也不会给出真正的响应码。
 *
 * 因此这三个调用必须作为一个**不可分割的整操作**，绝不能从外面拆开调用。
 * 所以本文件对外只暴露这一个函数，而不是把三步分别暴露出去
 * （最初我暴露了 `wpd_send_read_command_start` / `_finish`，正是这个错误，
 * 导致相机只回答了第一步的 0x0000，看起来像"命令不被支持"）。
 *
 * 成功返回最终的 MTP 响应码（0x2001 = OK）；返回 0 表示失败（错误见 *out_err）。
 */
WORD wpd_send_read_command(struct wpd_device *d, WORD code, const DWORD *args,
                           DWORD nargs, BYTE **out_data, DWORD *out_len,
                           char **out_err) {
    if (out_err) *out_err = NULL;
    *out_data = NULL;
    *out_len = 0;

    /* ---------- 第一步：告诉相机我们要读，取回上下文与总长度 ---------- */
    IPortableDeviceValues *params =
        build_command(&WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ,
                      code, args, nargs);
    if (!params) {
        if (out_err) *out_err = _strdup("创建参数包失败");
        return 0;
    }
    IPortableDeviceValues *first = send_command(d->device, params, out_err);
    params->lpVtbl->Release(params);
    if (!first) return 0;

    /* 注意：这一步的响应码**没有意义**，只记下来备用，不做判断 */
    LPWSTR context = NULL;
    first->lpVtbl->GetStringValue(first, &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT, &context);
    ULONGLONG total = 0;
    first->lpVtbl->GetUnsignedLargeIntegerValue(
        first, &WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE, &total);
    first->lpVtbl->Release(first);

    /* ---------- 第二步：读数据 ---------- */
    if (total > 0 && context) {
        IPortableDeviceValues *cmd = NULL;
        HRESULT hr = CoCreateInstance(&CLSID_PortableDeviceValues, NULL,
                                      CLSCTX_INPROC_SERVER,
                                      &IID_IPortableDeviceValues, (void **)&cmd);
        if (FAILED(hr) || !cmd) {
            if (out_err) *out_err = describe_hresult(hr);
            CoTaskMemFree(context);
            return 0;
        }

        BYTE *scratch = (BYTE *)xmalloc((size_t)total);
        cmd->lpVtbl->SetGuidValue(cmd, &WPD_PROPERTY_COMMON_COMMAND_CATEGORY,
                                  &WPD_CATEGORY_MTP_EXT_VENDOR_OPERATIONS);
        cmd->lpVtbl->SetUnsignedIntegerValue(cmd, &WPD_PROPERTY_COMMON_COMMAND_ID,
                                             WPD_COMMAND_MTP_EXT_READ_DATA.pid);
        cmd->lpVtbl->SetStringValue(cmd, &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT,
                                    context);
        cmd->lpVtbl->SetUnsignedLargeIntegerValue(
            cmd, &WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_TO_READ, total);
        cmd->lpVtbl->SetBufferValue(cmd, &WPD_PROPERTY_MTP_EXT_TRANSFER_DATA,
                                    scratch, (DWORD)total);

        IPortableDeviceValues *r = send_command(d->device, cmd, out_err);
        cmd->lpVtbl->Release(cmd);
        xfree(scratch);
        if (!r) {
            CoTaskMemFree(context);
            return 0;
        }

        BYTE *got = NULL;
        DWORD gotlen = 0;
        r->lpVtbl->GetBufferValue(r, &WPD_PROPERTY_MTP_EXT_TRANSFER_DATA, &got,
                                  &gotlen);
        r->lpVtbl->Release(r);

        if (got && gotlen > 0) {
            DWORD use = gotlen;
            if ((ULONGLONG)use > total) use = (DWORD)total;
            BYTE *copy = (BYTE *)xmalloc(use);
            if (copy) {
                memcpy(copy, got, use);
                *out_data = copy;
                *out_len = use;
            }
            CoTaskMemFree(got);
        }
    }

    /* ---------- 第三步：结束传输（必须做），这时响应码才有意义 ---------- */
    DWORD rc = 0;
    if (context) {
        IPortableDeviceValues *end = NULL;
        HRESULT hr = CoCreateInstance(&CLSID_PortableDeviceValues, NULL,
                                      CLSCTX_INPROC_SERVER,
                                      &IID_IPortableDeviceValues, (void **)&end);
        if (SUCCEEDED(hr) && end) {
            end->lpVtbl->SetGuidValue(end, &WPD_PROPERTY_COMMON_COMMAND_CATEGORY,
                                      &WPD_CATEGORY_MTP_EXT_VENDOR_OPERATIONS);
            end->lpVtbl->SetUnsignedIntegerValue(
                end, &WPD_PROPERTY_COMMON_COMMAND_ID,
                WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER.pid);
            end->lpVtbl->SetStringValue(
                end, &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT, context);

            IPortableDeviceValues *e = send_command(d->device, end, out_err);
            end->lpVtbl->Release(end);
            if (e) {
                e->lpVtbl->GetUnsignedIntegerValue(
                    e, &WPD_PROPERTY_MTP_EXT_RESPONSE_CODE, &rc);
                e->lpVtbl->Release(e);
            }
        }
        CoTaskMemFree(context);
    }

    if (rc == 0 && out_err && !*out_err) {
        *out_err = _strdup("相机没有返回响应码（可能是操作码不被支持，或相机不在应用安装模式）");
    }
    return (WORD)rc;
}
void wpd_free_buffer(BYTE *buf) {
    xfree(buf);
}
