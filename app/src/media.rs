//! Linen as a media player to the rest of the desktop (MPRIS over D-Bus): the
//! keyboard's play/pause keys work, and shells (Bloom's island, panels, lock
//! screens) show what's playing with its picture and controls.

use std::sync::mpsc::Sender;
use std::time::Duration;

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection};

use crate::Msg;

/// What the desktop asked for.
#[derive(Debug, Clone)]
pub enum Command {
    Toggle,
    Play,
    Pause,
    Stop,
    /// seconds, relative
    SeekBy(f64),
    /// seconds, absolute
    SeekTo(f64),
    Raise,
}

pub struct Media {
    controls: MediaControls,
}

impl Media {
    pub fn start(tx: Sender<Msg>) -> Option<Self> {
        let config = PlatformConfig { dbus_name: "linen", display_name: "Linen", hwnd: None };
        let mut controls = MediaControls::new(config).map_err(|e| log::warn!("media keys: {e:?}")).ok()?;
        controls
            .attach(move |e: MediaControlEvent| {
                let cmd = match e {
                    MediaControlEvent::Toggle => Command::Toggle,
                    MediaControlEvent::Play => Command::Play,
                    MediaControlEvent::Pause => Command::Pause,
                    MediaControlEvent::Stop => Command::Stop,
                    MediaControlEvent::Seek(SeekDirection::Forward) => Command::SeekBy(10.0),
                    MediaControlEvent::Seek(SeekDirection::Backward) => Command::SeekBy(-10.0),
                    MediaControlEvent::SeekBy(SeekDirection::Forward, d) => Command::SeekBy(d.as_secs_f64()),
                    MediaControlEvent::SeekBy(SeekDirection::Backward, d) => Command::SeekBy(-d.as_secs_f64()),
                    MediaControlEvent::SetPosition(MediaPosition(p)) => Command::SeekTo(p.as_secs_f64()),
                    MediaControlEvent::Raise => Command::Raise,
                    _ => return,
                };
                let _ = tx.send(Msg::Media(cmd));
            })
            .map_err(|e| log::warn!("media keys: {e:?}"))
            .ok()?;
        Some(Self { controls })
    }

    /// A video started: its title, channel, picture and length.
    pub fn now_playing(&mut self, id: &str, title: &str, channel: &str, duration: f64) {
        let art = format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg");
        let _ = self.controls.set_metadata(MediaMetadata {
            title: Some(title),
            artist: Some(channel),
            album: None,
            cover_url: Some(&art),
            duration: (duration > 0.0).then(|| Duration::from_secs_f64(duration)),
        });
    }

    pub fn state(&mut self, playing: bool, position: f64) {
        let progress = Some(MediaPosition(Duration::from_secs_f64(position.max(0.0))));
        let _ = self.controls.set_playback(if playing { MediaPlayback::Playing { progress } } else { MediaPlayback::Paused { progress } });
    }

    pub fn stopped(&mut self) {
        let _ = self.controls.set_playback(MediaPlayback::Stopped);
    }
}
