//! Transport buttons drive whatever is playing on the PC, through the same
//! media keys a keyboard sends. The HomePod only ever sees the mixed output.
//! Playback state and the track title come back from Windows' own media
//! session (the thing the volume flyout shows).

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager, GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_MEDIA_NEXT_TRACK,
    VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK,
};

#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Previous,
    PlayPause,
    Next,
}

fn tap(vk: VIRTUAL_KEY) {
    let make = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let inputs = [make(Default::default()), make(KEYEVENTF_KEYUP)];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Drive the current media session directly; media keys only as a fallback.
/// A key press is routed through the foreground window, and while the panel
/// is focused that window is our own WebView, which swallows it.
pub fn send(t: Transport) {
    let via_session = || -> windows::core::Result<bool> {
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.get()?;
        let session = manager.GetCurrentSession()?;
        let op = match t {
            Transport::Previous => session.TrySkipPreviousAsync()?,
            Transport::PlayPause => session.TryTogglePlayPauseAsync()?,
            Transport::Next => session.TrySkipNextAsync()?,
        };
        op.get()
    };
    if via_session().unwrap_or(false) {
        return;
    }
    tap(match t {
        Transport::Previous => VK_MEDIA_PREV_TRACK,
        Transport::PlayPause => VK_MEDIA_PLAY_PAUSE,
        Transport::Next => VK_MEDIA_NEXT_TRACK,
    })
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct NowPlaying {
    pub playing: bool,
    pub title: String,
    pub artist: String,
}

/// What Windows' media session reports for the current app, or empty when
/// nothing has registered one (a bare browser tab without media metadata,
/// or silence).
pub fn now_playing() -> NowPlaying {
    let inner = || -> windows::core::Result<NowPlaying> {
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.get()?;
        let session = manager.GetCurrentSession()?;
        let playing = session.GetPlaybackInfo()?.PlaybackStatus()? == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing;
        let props = session.TryGetMediaPropertiesAsync()?.get()?;
        Ok(NowPlaying { playing, title: props.Title()?.to_string(), artist: props.Artist()?.to_string() })
    };
    inner().unwrap_or_default()
}
