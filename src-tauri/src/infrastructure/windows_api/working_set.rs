use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_QUERY_INFORMATION,
    PROCESS_SET_QUOTA,
};

const WEBVIEW_PROCESS_NAME: &str = "msedgewebview2.exe";

struct ProcessNode {
    pid: u32,
    parent_pid: u32,
}

/// 把主进程和整组 WebView2 进程的物理页交还给系统
pub fn trim_process_tree() {
    unsafe {
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }

    for pid in webview_descendants(unsafe { GetCurrentProcessId() }) {
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_SET_QUOTA, false, pid)
            else {
                continue;
            };
            let _ = EmptyWorkingSet(handle);
            let _ = CloseHandle(handle);
        }
    }
}

/// WebView2 的 renderer / GPU / crashpad 进程挂在 browser 进程下面，而 browser 进程才是宿主
/// 进程的直接子进程，所以必须沿着进程树一层层往下走。
fn webview_descendants(root_pid: u32) -> Vec<u32> {
    let nodes = snapshot_webview_processes();
    let mut found: Vec<u32> = Vec::new();
    let mut pending = vec![root_pid];

    while let Some(parent_pid) = pending.pop() {
        for node in &nodes {
            if node.parent_pid != parent_pid || node.pid == root_pid || found.contains(&node.pid) {
                continue;
            }
            found.push(node.pid);
            pending.push(node.pid);
        }
    }

    found
}

fn snapshot_webview_processes() -> Vec<ProcessNode> {
    let mut nodes = Vec::new();

    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return nodes;
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if is_webview_process(&entry.szExeFile) {
                    nodes.push(ProcessNode {
                        pid: entry.th32ProcessID,
                        parent_pid: entry.th32ParentProcessID,
                    });
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }

    nodes
}

fn is_webview_process(exe_file: &[u16]) -> bool {
    let len = exe_file
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(exe_file.len());
    String::from_utf16_lossy(&exe_file[..len]).eq_ignore_ascii_case(WEBVIEW_PROCESS_NAME)
}
