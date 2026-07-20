//! Native status sounds, played from Rust via `rodio` (talks to the system
//! audio backend directly). This deliberately bypasses the WebView: on Linux,
//! WebKitGTK routes all HTML/Web-Audio playback through GStreamer's
//! `autoaudiosink`, which isn't always installed — so a webview `<audio>`
//! element silently fails (and its init has crashed the process). Playing here
//! keeps sound self-contained and can never take the window down.

use std::io::Cursor;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::time::Duration;

/// The completion "click" sample, embedded into the binary.
const CLICK_MP3: &[u8] = include_bytes!("../assets/click.mp3");

#[derive(Clone, Copy)]
pub enum Sound {
    /// Task finished (running → idle): the click sample.
    Click,
    /// Needs approval (→ permission): two short beeps.
    Approval,
}

static TX: OnceLock<Sender<Sound>> = OnceLock::new();

/// Spawn the dedicated audio thread. It owns the output stream for its whole
/// life (the stream stops the moment it's dropped) and plays each request as
/// it arrives. If no audio device is available it just drains the channel.
pub fn init() {
    let (tx, rx) = mpsc::channel::<Sound>();
    if TX.set(tx).is_err() {
        return; // already initialised
    }
    std::thread::spawn(move || {
        let stream = match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("audio unavailable, sounds disabled: {e}");
                for _ in rx {} // keep draining so senders never block
                return;
            }
        };
        let mixer = stream.mixer();
        for sound in rx {
            let sink = rodio::Player::connect_new(mixer);
            match sound {
                Sound::Click => {
                    if let Ok(dec) = rodio::Decoder::new(Cursor::new(CLICK_MP3)) {
                        sink.append(dec);
                    }
                }
                Sound::Approval => {
                    use rodio::source::{SineWave, Source};
                    let beep = |delay_ms: u64| {
                        SineWave::new(520.0)
                            .take_duration(Duration::from_millis(100))
                            .amplify(0.20)
                            .delay(Duration::from_millis(delay_ms))
                    };
                    sink.append(beep(0));
                    sink.append(beep(40));
                }
            }
            // Detach so playback continues while we wait for the next request.
            sink.detach();
        }
    });
}

/// Queue a sound. No-op if the audio thread never started or the device is gone.
pub fn play(sound: Sound) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(sound);
    }
}
