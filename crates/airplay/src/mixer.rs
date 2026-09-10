//! The Windows volume mixer, per app: mute or unmute every audio session that
//! belongs to a process tree. DeetsMusic uses it so the PC goes quiet while a
//! speaker plays (the per-process loopback is a tap; the local output keeps
//! playing otherwise). Only the app's own sessions are touched, never the
//! master volume or another app.
//!
//! Sessions are enumerated on the default render device. A session is matched
//! by process id against the tree under `root` (the WebView2 children own the
//! actual audio sessions, not the host exe).

use std::collections::{HashMap, HashSet};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS};
use windows::core::Interface;

/// Every process: (pid, parent pid, exe name lower-cased).
fn snapshot() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    unsafe {
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let mut e = PROCESSENTRY32 { dwSize: std::mem::size_of::<PROCESSENTRY32>() as u32, ..Default::default() };
            if Process32First(snap, &mut e).is_ok() {
                loop {
                    let name: String = e.szExeFile.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect();
                    out.push((e.th32ProcessID, e.th32ParentProcessID, name.to_ascii_lowercase()));
                    if Process32Next(snap, &mut e).is_err() {
                        break;
                    }
                }
            }
            CloseHandle(snap).ok();
        }
    }
    out
}

/// The direct children of `root` whose exe is `name` (case-insensitive), e.g.
/// the WebView2 browser process under a Tauri app. Windows' process-loopback
/// "include tree" does NOT reach from a host exe into its WebView2 children
/// (measured 2026-09-10: capturing deetsmusic.exe heard nothing, capturing its
/// msedgewebview2.exe child heard the song), so a capture must target the child.
pub fn children_named(root: u32, name: &str) -> Vec<u32> {
    let name = name.to_ascii_lowercase();
    snapshot().into_iter().filter(|(_, parent, exe)| *parent == root && *exe == name).map(|(pid, _, _)| pid).collect()
}

/// `root` and every descendant process, by id.
pub fn process_tree(root: u32) -> HashSet<u32> {
    let parent_of: HashMap<u32, u32> = snapshot().into_iter().map(|(pid, parent, _)| (pid, parent)).collect();
    let mut tree = HashSet::from([root]);
    // Children can appear before parents in the snapshot; a few passes settle it.
    loop {
        let before = tree.len();
        for (pid, parent) in &parent_of {
            if tree.contains(parent) {
                tree.insert(*pid);
            }
        }
        if tree.len() == before {
            break;
        }
    }
    tree
}

/// Mute (or unmute) every audio session owned by `root`'s process tree.
/// Returns how many sessions were touched.
pub fn set_tree_mute(root: u32, muted: bool) -> Result<usize, String> {
    let tree = process_tree(root);
    let mut touched = 0;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| format!("default render device: {e}"))?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioSessionManager2: {e}"))?;
        let sessions = manager.GetSessionEnumerator().map_err(|e| format!("GetSessionEnumerator: {e}"))?;
        let n = sessions.GetCount().map_err(|e| e.to_string())?;
        for i in 0..n {
            let Ok(ctl) = sessions.GetSession(i) else { continue };
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
            let Ok(pid) = ctl2.GetProcessId() else { continue };
            if !tree.contains(&pid) {
                continue;
            }
            let Ok(vol) = ctl2.cast::<ISimpleAudioVolume>() else { continue };
            if vol.SetMute(muted, std::ptr::null()).is_ok() {
                touched += 1;
            }
        }
    }
    Ok(touched)
}
