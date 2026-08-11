use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager};
use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Controller, ICoreWebView2_3};
use webview2_com::TrySuspendCompletedHandler;
use windows::core::Interface;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindowVisible, PostMessageW, RegisterWindowMessageW, SWP_HIDEWINDOW, SWP_SHOWWINDOW,
    WINDOWPOS, WM_DESTROY, WM_SHOWWINDOW, WM_WINDOWPOSCHANGING,
};

use super::working_set;

/// 隐藏后等待多久才回收；用户快速反复切换时靠它把来回换页压掉
const HIDE_RELEASE_DELAY: Duration = Duration::from_secs(3);
/// TrySuspend 是异步的，等 renderer 真的挂起再回收工作集才有意义
const SUSPEND_SETTLE_DELAY: Duration = Duration::from_millis(500);
const SUBCLASS_ID: usize = 1338;

static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static MAIN_HWND: AtomicUsize = AtomicUsize::new(0);
static CONTROLLER: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static CORE_WEBVIEW: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static RELEASE_MSG: AtomicU32 = AtomicU32::new(0);
static RELEASE_GENERATION: AtomicUsize = AtomicUsize::new(0);
static IS_RELEASED: AtomicBool = AtomicBool::new(false);

/// 幂等，可以从任意窗口相关入口重复调用
pub fn install(app: &AppHandle) {
    if HOOK_INSTALLED.load(Ordering::Acquire) {
        return;
    }

    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let Ok(handle) = window.hwnd() else {
        return;
    };
    if HOOK_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }

    MAIN_HWND.store(handle.0 as usize, Ordering::Release);
    unsafe {
        RELEASE_MSG.store(
            RegisterWindowMessageW(windows::core::w!("TiezIdleMemoryRelease")),
            Ordering::Release,
        );
    }

    // WebView2 有线程亲和性，控制器的获取、子类化以及后续所有挂起/恢复调用都必须发生在
    // 创建 webview 的那个线程上，`with_webview` 的闭包正好在那里执行。
    let dispatched = window.with_webview(|platform| unsafe {
        let controller = platform.controller();
        if let Ok(core) = controller.CoreWebView2() {
            if let Ok(core) = core.cast::<ICoreWebView2_3>() {
                CORE_WEBVIEW.store(core.into_raw(), Ordering::Release);
            }
        }
        CONTROLLER.store(controller.into_raw(), Ordering::Release);

        let hwnd = HWND(MAIN_HWND.load(Ordering::Acquire) as *mut c_void);
        let _ = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0);
    });

    if dispatched.is_err() {
        HOOK_INSTALLED.store(false, Ordering::Release);
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    match msg {
        WM_WINDOWPOSCHANGING => {
            let position = lparam.0 as *const WINDOWPOS;
            if !position.is_null() {
                let flags = unsafe { (*position).flags };
                if flags.contains(SWP_SHOWWINDOW) {
                    restore_webview();
                } else if flags.contains(SWP_HIDEWINDOW) {
                    schedule_release();
                }
            }
        }
        WM_SHOWWINDOW => {
            if wparam.0 != 0 {
                restore_webview();
            } else {
                schedule_release();
            }
        }
        WM_DESTROY => teardown(hwnd),
        _ => {
            let release_msg = RELEASE_MSG.load(Ordering::Acquire);
            if release_msg != 0 && msg == release_msg {
                suspend_webview(wparam.0);
                return LRESULT(0);
            }
        }
    }

    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

fn schedule_release() {
    let generation = RELEASE_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;

    std::thread::spawn(move || {
        std::thread::sleep(HIDE_RELEASE_DELAY);
        if !request_suspend(generation) {
            return;
        }

        std::thread::sleep(SUSPEND_SETTLE_DELAY);
        if RELEASE_GENERATION.load(Ordering::Acquire) != generation {
            return;
        }
        working_set::trim_process_tree();
    });
}

fn request_suspend(generation: usize) -> bool {
    if RELEASE_GENERATION.load(Ordering::Acquire) != generation {
        return false;
    }

    let hwnd = MAIN_HWND.load(Ordering::Acquire);
    let release_msg = RELEASE_MSG.load(Ordering::Acquire);
    if hwnd == 0 || release_msg == 0 {
        return false;
    }

    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
        if IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let _ = PostMessageW(Some(hwnd), release_msg, WPARAM(generation), LPARAM(0));
    }
    true
}

fn suspend_webview(generation: usize) {
    if RELEASE_GENERATION.load(Ordering::Acquire) != generation {
        return;
    }

    let hwnd = HWND(MAIN_HWND.load(Ordering::Acquire) as *mut c_void);
    if hwnd.0.is_null() || unsafe { IsWindowVisible(hwnd).as_bool() } {
        return;
    }

    unsafe {
        let Some(controller) = controller() else {
            return;
        };
        // TrySuspend 只在控制器不可见时才被接受，而 wry 不会因为宿主窗口隐藏就同步这个状态
        if controller.SetIsVisible(false).is_err() {
            return;
        }
        IS_RELEASED.store(true, Ordering::Release);

        if let Some(core) = core_webview() {
            let handler = TrySuspendCompletedHandler::create(Box::new(|_, _| Ok(())));
            let _ = core.TrySuspend(&handler);
        }
    }
}

fn restore_webview() {
    RELEASE_GENERATION.fetch_add(1, Ordering::AcqRel);
    if !IS_RELEASED.swap(false, Ordering::AcqRel) {
        return;
    }

    unsafe {
        if let Some(core) = core_webview() {
            let _ = core.Resume();
        }
        if let Some(controller) = controller() {
            let _ = controller.SetIsVisible(true);
        }
    }
}

fn teardown(hwnd: HWND) {
    RELEASE_GENERATION.fetch_add(1, Ordering::AcqRel);
    MAIN_HWND.store(0, Ordering::Release);
    IS_RELEASED.store(false, Ordering::Release);

    let core = CORE_WEBVIEW.swap(null_mut(), Ordering::AcqRel);
    if !core.is_null() {
        drop(unsafe { ICoreWebView2_3::from_raw(core) });
    }
    let controller = CONTROLLER.swap(null_mut(), Ordering::AcqRel);
    if !controller.is_null() {
        drop(unsafe { ICoreWebView2Controller::from_raw(controller) });
    }

    unsafe {
        let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
    }
}

unsafe fn controller() -> Option<ManuallyDrop<ICoreWebView2Controller>> {
    let ptr = CONTROLLER.load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    Some(ManuallyDrop::new(unsafe {
        ICoreWebView2Controller::from_raw(ptr)
    }))
}

unsafe fn core_webview() -> Option<ManuallyDrop<ICoreWebView2_3>> {
    let ptr = CORE_WEBVIEW.load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    Some(ManuallyDrop::new(unsafe { ICoreWebView2_3::from_raw(ptr) }))
}
