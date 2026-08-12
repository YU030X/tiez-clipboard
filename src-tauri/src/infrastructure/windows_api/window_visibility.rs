use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindowVisible, SWP_HIDEWINDOW, SWP_SHOWWINDOW, WINDOWPOS, WM_DESTROY, WM_SHOWWINDOW,
    WM_WINDOWPOSCHANGING,
};

use super::working_set;

/// 隐藏后等待多久才回收；用户快速反复切换时靠它把来回换页压掉
const HIDE_RELEASE_DELAY: Duration = Duration::from_secs(3);
const SUBCLASS_ID: usize = 1338;

/// 主窗口重新显示。前端只在挂载时全量拉取一次历史，之后靠 `clipboard-updated` 增量维护，
/// 任何一次事件丢失都会让列表永久缺内容，所以每次显示时补一次全量兜底。
pub const MAIN_WINDOW_SHOWN_EVENT: &str = "main-window-shown";

static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static MAIN_HWND: AtomicUsize = AtomicUsize::new(0);
static MAIN_WINDOW_HIDDEN: AtomicBool = AtomicBool::new(false);
static RELEASE_GENERATION: AtomicUsize = AtomicUsize::new(0);

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

    // tauri 重导出的 HWND 与本地 windows crate 不是同一个类型，统一退化成裸指针值传递
    let hwnd_value = handle.0 as usize;
    MAIN_HWND.store(hwnd_value, Ordering::Release);

    // 子类化必须发生在拥有该窗口消息队列的线程上，也就是事件循环所在的主线程。
    // 初始可见性也在这里取：与子类化同处一个闭包、同一个线程，中间插不进窗口消息，
    // 标志位不会因为派发延迟而漏掉一次显示或隐藏。
    let dispatched = app.run_on_main_thread(move || {
        MAIN_WINDOW_HIDDEN.store(!is_main_window_visible(), Ordering::Release);
        unsafe {
            let hwnd = HWND(hwnd_value as *mut c_void);
            let _ = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0);
        }
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
                    on_shown();
                } else if flags.contains(SWP_HIDEWINDOW) {
                    on_hidden();
                }
            }
        }
        WM_SHOWWINDOW => {
            if wparam.0 != 0 {
                on_shown();
            } else {
                on_hidden();
            }
        }
        WM_DESTROY => teardown(hwnd),
        _ => {}
    }

    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// 单次显示会同时走 WM_WINDOWPOSCHANGING 和 WM_SHOWWINDOW，只在真正的状态翻转上做事
fn on_shown() {
    if !MAIN_WINDOW_HIDDEN.swap(false, Ordering::AcqRel) {
        return;
    }
    RELEASE_GENERATION.fetch_add(1, Ordering::AcqRel);

    if let Some(app) = crate::GLOBAL_APP_HANDLE.get() {
        let _ = app.emit(MAIN_WINDOW_SHOWN_EVENT, ());
    }
}

fn on_hidden() {
    if MAIN_WINDOW_HIDDEN.swap(true, Ordering::AcqRel) {
        return;
    }
    schedule_release();
}

fn schedule_release() {
    let generation = RELEASE_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;

    std::thread::spawn(move || {
        std::thread::sleep(HIDE_RELEASE_DELAY);
        if RELEASE_GENERATION.load(Ordering::Acquire) != generation {
            return;
        }
        // 期间又被显示出来就别回收，否则刚交还的页马上要换回来
        if is_main_window_visible() {
            return;
        }

        working_set::trim_process_tree();
    });
}

fn is_main_window_visible() -> bool {
    let hwnd = MAIN_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        return false;
    }
    unsafe { IsWindowVisible(HWND(hwnd as *mut c_void)).as_bool() }
}

fn teardown(hwnd: HWND) {
    RELEASE_GENERATION.fetch_add(1, Ordering::AcqRel);
    MAIN_HWND.store(0, Ordering::Release);

    unsafe {
        let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
    }
}
