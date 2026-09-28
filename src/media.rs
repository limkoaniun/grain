//! Media: resolving embed targets to vault files, decoding and encoding images
//! for the terminal, and audio playback.
//!
//! This module owns everything between an `![[...]]` target and what the review
//! screen shows or plays. Resolution lives here (`MediaIndex`), and so do the
//! extension lists that decide whether a target is an image, a sound or neither
//! (`kind_of`). `ui/` never touches a file; it only draws what `App` already holds.

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
#[cfg(test)]
use anyhow::anyhow;
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::Resize;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};

use crate::vault::card::Segment;
use crate::vault::scan::scan_media;

/// Rows an image is given on the review screen.
pub const IMAGE_ROWS: u16 = 8;

/// What a target can be shown or played as, decided by its extension.
///
/// The lists are the ones grain can actually handle: what the `image` crate
/// decodes, and the codecs enabled on `rodio`. Everything else is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Audio,
    Other,
}

/// The kind of an embed target, by its file extension, case-insensitively.
pub fn kind_of(target: &str) -> MediaKind {
    let ext = target
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => MediaKind::Image,
        "mp3" | "wav" | "ogg" | "flac" => MediaKind::Audio,
        _ => MediaKind::Other,
    }
}

/// Every non-markdown file in the vault, for resolving embed targets the way
/// Obsidian does: a path if it names one, otherwise the first file with that name.
#[derive(Debug, Clone)]
pub struct MediaIndex {
    root: PathBuf,
    paths: Vec<String>,
}

impl MediaIndex {
    /// An index over `paths`, which are vault-relative and `/`-separated.
    pub fn new(root: &Path, paths: Vec<String>) -> Self {
        Self { root: root.to_path_buf(), paths }
    }

    /// Walk the vault for its media files and index them.
    pub fn from_vault(root: &Path) -> Result<Self> {
        Ok(Self::new(root, scan_media(root)?))
    }

    /// The vault-relative path an embed target names, or `None`.
    ///
    /// An exact path wins if a file exists there; otherwise the first indexed
    /// file whose name matches, in walk order.
    pub fn resolve(&self, target: &str) -> Option<String> {
        if self.abs(target).is_file() {
            return Some(target.to_string());
        }
        let name = file_name(target);
        self.paths.iter().find(|p| file_name(p) == name).cloned()
    }

    /// The absolute path of a vault-relative media path.
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// One embed of a card side, ready for the screen.
///
/// An image carries its encoded `Protocol` when there was a file to decode and
/// room to draw it; otherwise a `note` says why, and the review screen falls
/// back to the placeholder line. An audio embed carries the vault-relative path
/// to play, and remembers a play that failed so the line can say so.
pub enum Media {
    Image {
        target: String,
        protocol: Option<Protocol>,
        note: Option<&'static str>,
    },
    Audio {
        target: String,
        path: Option<String>,
        failed: bool,
    },
    Other {
        target: String,
    },
}

// `Protocol` derives neither `Debug` nor `PartialEq`, so `Media` prints the
// encoded size instead of the pixels and is never compared as a whole.
impl fmt::Debug for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Media::Image { target, protocol, note } => f
                .debug_struct("Image")
                .field("target", target)
                .field("protocol", &protocol.as_ref().map(|p| p.size()))
                .field("note", note)
                .finish(),
            Media::Audio { target, path, failed } => f
                .debug_struct("Audio")
                .field("target", target)
                .field("path", path)
                .field("failed", failed)
                .finish(),
            Media::Other { target } => f.debug_struct("Other").field("target", target).finish(),
        }
    }
}

/// The media of both sides of one card, in embed order.
#[derive(Debug, Default)]
pub struct SideMedia {
    pub question: Vec<Media>,
    pub answer: Vec<Media>,
}

impl SideMedia {
    /// Whether either side has an audio embed, playable or not.
    #[cfg(test)]
    pub fn has_audio(&self) -> bool {
        self.question
            .iter()
            .chain(self.answer.iter())
            .any(|m| matches!(m, Media::Audio { .. }))
    }
}

/// Resolve and prepare every embed of one card side, keeping embed order.
///
/// Images are decoded and encoded for the terminal here, once per card load:
/// `ui/` only draws a `Protocol` this produced. Text segments are skipped.
pub fn load_side(
    picker: &Picker,
    index: &MediaIndex,
    segments: &[Segment],
    image_box: Size,
) -> Vec<Media> {
    segments
        .iter()
        .filter_map(|segment| match segment {
            Segment::Text(_) => None,
            Segment::Embed(embed) => Some(load_embed(picker, index, &embed.target, image_box)),
        })
        .collect()
}

/// Resolve and prepare one embed target, by its kind.
///
/// The per-embed half of `load_side`, so a caller with a single target — the
/// read screen's picture paragraphs — gets the same `Media` a card side would.
pub fn load_embed(picker: &Picker, index: &MediaIndex, target: &str, image_box: Size) -> Media {
    match kind_of(target) {
        MediaKind::Image => load_image(picker, index, target, image_box),
        MediaKind::Audio => Media::Audio {
            target: target.to_string(),
            path: index.resolve(target),
            failed: false,
        },
        MediaKind::Other => Media::Other { target: target.to_string() },
    }
}

/// Decode an image target and encode it for `image_box`.
///
/// Every failure becomes a note rather than an error: a card with a broken
/// embed still reviews, with the reason on the placeholder line.
fn load_image(picker: &Picker, index: &MediaIndex, target: &str, image_box: Size) -> Media {
    let note = |note: Option<&'static str>| Media::Image {
        target: target.to_string(),
        protocol: None,
        note,
    };
    let Some(rel) = index.resolve(target) else {
        return note(Some("not found"));
    };
    let Ok(decoded) = image::open(index.abs(&rel)) else {
        return note(Some("cannot decode"));
    };
    if image_box.width < 2 || image_box.height < 2 {
        return note(None);
    }
    match picker.new_protocol(decoded, image_box, Resize::Fit(None)) {
        Ok(protocol) => Media::Image {
            target: target.to_string(),
            protocol: Some(protocol),
            note: None,
        },
        Err(_) => note(Some("cannot decode")),
    }
}

/// Playing one sound at a time, behind a trait so the review loop can be tested
/// without a sound card.
///
/// There is deliberately no `Send` bound: `MixerDeviceSink` holds a
/// `cpal::Stream` that may be `!Send`, and the implementation lives on `App`,
/// which never leaves the UI thread.
pub trait Audio {
    /// Queue `path` behind whatever is already playing, so the sounds of a card
    /// play back to back. A caller that wants replacement calls `stop` first.
    /// Any failure is an error for the caller to show; it is never fatal.
    fn play(&mut self, path: &Path) -> Result<()>;
    /// Silence whatever is playing. Never fails.
    fn stop(&mut self);
    /// Whether a sound started here is still running.
    fn is_playing(&self) -> bool;
}

/// Playback over rodio, one `Player` at a time.
///
/// The device is opened lazily on the first `play` and kept alive afterwards:
/// dropping `MixerDeviceSink` stops playback, and opening one costs enough that
/// a card with no sound should not pay for it. Dropping `RodioAudio` drops the
/// device, which is the only stop that matters at shutdown — silently, because
/// the open turns rodio's drop notice off.
pub struct RodioAudio {
    device: Option<MixerDeviceSink>,
    player: Option<Player>,
}

impl RodioAudio {
    /// Nothing is opened until the first `play`.
    pub fn new() -> Self {
        RodioAudio { device: None, player: None }
    }
}

impl Default for RodioAudio {
    fn default() -> Self {
        Self::new()
    }
}

impl Audio for RodioAudio {
    fn play(&mut self, path: &Path) -> Result<()> {
        // Taking and re-inserting keeps the lazy open to one borrow of `device`;
        // a failed open leaves the field `None`, so the next play tries again.
        let device = match self.device.take() {
            Some(device) => device,
            None => {
                let mut device = DeviceSinkBuilder::open_default_sink()
                    .context("opening the default audio device")?;
                // A dropped sink otherwise `eprintln!`s a 140-character notice
                // (rodio stream.rs:83-91, on by default at stream.rs:504). In a
                // TUI that lands on top of the drawn screen; the `tracing`
                // feature that would reroute it is deliberately off.
                device.log_on_drop(false);
                device
            }
        };
        let device = self.device.insert(device);

        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let decoder =
            Decoder::try_from(file).with_context(|| format!("decoding {}", path.display()))?;

        // Appending to the player already running is what puts two sounds of one
        // card back to back. `stop` takes the player out, so `append` never meets a
        // stopped one — the branch in `Player::append` that waits for the queue to
        // flush (rodio player.rs:104-116) is never reached, and nothing blocks here.
        let player = match self.player.take() {
            Some(player) => player,
            None => Player::connect_new(device.mixer()),
        };
        player.append(decoder);
        player.play();
        self.player = Some(player);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(player) = self.player.take() {
            player.stop();
        }
    }

    fn is_playing(&self) -> bool {
        self.player.as_ref().is_some_and(|player| !player.empty())
    }
}

/// What a `NullAudio` was asked to do, and what it should pretend happened.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct AudioLog {
    /// One entry per successful `play`, in order.
    pub plays: Vec<String>,
    /// How many times `stop` was called.
    pub stops: usize,
    /// What `is_playing` answers.
    pub playing: bool,
    /// When set, `play` fails with this message instead of recording.
    pub fail_with: Option<String>,
}

/// An `Audio` that opens no device: it records calls into a shared log and
/// answers from it. Tests hold the other end of the `Arc`.
#[cfg(test)]
pub struct NullAudio(Arc<Mutex<AudioLog>>);

#[cfg(test)]
impl NullAudio {
    /// The backend and the log it writes to.
    pub fn new() -> (Self, Arc<Mutex<AudioLog>>) {
        let log = Arc::new(Mutex::new(AudioLog::default()));
        (NullAudio(Arc::clone(&log)), log)
    }
}

#[cfg(test)]
impl Audio for NullAudio {
    fn play(&mut self, path: &Path) -> Result<()> {
        let mut log = self.0.lock().map_err(|_| anyhow!("audio log poisoned"))?;
        if let Some(reason) = &log.fail_with {
            return Err(anyhow!(reason.clone()));
        }
        log.plays.push(path.display().to_string());
        Ok(())
    }

    fn stop(&mut self) {
        if let Ok(mut log) = self.0.lock() {
            log.stops += 1;
        }
    }

    fn is_playing(&self) -> bool {
        self.0.lock().map(|log| log.playing).unwrap_or(false)
    }
}

/// A picker that encodes halfblocks whatever the terminal is, for tests.
///
/// `Picker::from_fontsize` would read the iTerm2 environment variables and pick
/// that protocol on a developer machine; it is also deprecated in 11.1.0.
/// `Picker::halfblocks` is "guaranteed to only work with Halfblocks" and fixes
/// the font size at 10x20, so encoded sizes are the same everywhere.
#[cfg(test)]
pub fn test_picker() -> Picker {
    Picker::halfblocks()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::vault::card::Embed;
    use std::fs;

    fn embed(target: &str) -> Segment {
        Segment::Embed(Embed { target: target.to_string() })
    }

    /// A temp copy of `fixtures/vault`, with a real image written over the
    /// checked-in `media/pomelo.png` (which is a zero-byte placeholder) and a
    /// `bad.png` that is text, not an image.
    fn temp_vault() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault");
        for entry in walkdir::WalkDir::new(&src)
            .into_iter()
            .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
        {
            let entry = entry.unwrap();
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(&src).unwrap();
            let dest = dir.path().join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::copy(entry.path(), &dest).unwrap();
        }
        // Large enough that fitting it into the review box has to shrink it.
        let img = image::RgbImage::from_fn(400, 300, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        image::DynamicImage::ImageRgb8(img)
            .save(dir.path().join("media/pomelo.png"))
            .unwrap();
        fs::write(dir.path().join("bad.png"), "not an image").unwrap();
        dir
    }

    #[test]
    fn load_side_encodes_pomelo_png_within_the_box() {
        let dir = temp_vault();
        let index = MediaIndex::from_vault(dir.path()).unwrap();
        let segments = vec![Segment::Text("Q?".to_string()), embed("pomelo.png")];

        let media = load_side(&test_picker(), &index, &segments, Size { width: 78, height: 8 });

        assert_eq!(media.len(), 1, "text segments are skipped: {media:?}");
        match &media[0] {
            Media::Image { target, protocol, note } => {
                assert_eq!(target, "pomelo.png");
                assert_eq!(*note, None);
                let size = protocol.as_ref().unwrap().size();
                assert!(size.width >= 1 && size.width <= 78, "width {}", size.width);
                assert!(size.height >= 1 && size.height <= 8, "height {}", size.height);
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn load_side_marks_missing_and_undecodable() {
        let dir = temp_vault();
        let index = MediaIndex::from_vault(dir.path()).unwrap();
        let box_size = Size { width: 78, height: 8 };

        let media = load_side(&test_picker(), &index, &[embed("nope.png")], box_size);
        match &media[0] {
            Media::Image { target, protocol, note } => {
                assert_eq!(target, "nope.png");
                assert!(protocol.is_none());
                assert_eq!(*note, Some("not found"));
            }
            other => panic!("expected an image, got {other:?}"),
        }

        let media = load_side(&test_picker(), &index, &[embed("bad.png")], box_size);
        match &media[0] {
            Media::Image { target, protocol, note } => {
                assert_eq!(target, "bad.png");
                assert!(protocol.is_none());
                assert_eq!(*note, Some("cannot decode"));
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn load_side_keeps_audio_and_other() {
        let dir = temp_vault();
        let index = MediaIndex::from_vault(dir.path()).unwrap();
        let segments = vec![embed("pomelo.mp3"), embed("ghost.mp3"), embed("x.svg")];

        let media = load_side(&test_picker(), &index, &segments, Size { width: 78, height: 8 });

        assert_eq!(media.len(), 3);
        match &media[0] {
            Media::Audio { target, path, failed } => {
                assert_eq!(target, "pomelo.mp3");
                assert_eq!(path.as_deref(), Some("media/pomelo.mp3"));
                assert!(!failed);
            }
            other => panic!("expected audio, got {other:?}"),
        }
        match &media[1] {
            Media::Audio { target, path, .. } => {
                assert_eq!(target, "ghost.mp3");
                assert_eq!(*path, None);
            }
            other => panic!("expected audio, got {other:?}"),
        }
        match &media[2] {
            Media::Other { target } => assert_eq!(target, "x.svg"),
            other => panic!("expected other, got {other:?}"),
        }

        let side = SideMedia { question: media, answer: Vec::new() };
        assert!(side.has_audio());
        assert!(!SideMedia::default().has_audio());
    }

    #[test]
    fn load_side_box_too_small_gives_no_protocol() {
        let dir = temp_vault();
        let index = MediaIndex::from_vault(dir.path()).unwrap();
        let segments = vec![embed("pomelo.png")];

        for box_size in [Size { width: 78, height: 1 }, Size { width: 1, height: 8 }] {
            let media = load_side(&test_picker(), &index, &segments, box_size);
            match &media[0] {
                Media::Image { protocol, note, .. } => {
                    assert!(protocol.is_none(), "box {box_size:?} encoded anyway");
                    assert_eq!(*note, None);
                }
                other => panic!("expected an image, got {other:?}"),
            }
        }
    }

    #[test]
    fn load_embed_matches_load_side_per_kind() {
        let dir = temp_vault();
        let index = MediaIndex::from_vault(dir.path()).unwrap();
        let picker = test_picker();
        let box_size = Size { width: 78, height: 8 };

        match load_embed(&picker, &index, "buddhas-hand.jpg", box_size) {
            Media::Image { target, protocol, note } => {
                assert_eq!(target, "buddhas-hand.jpg");
                assert_eq!(note, None);
                let size = protocol.unwrap().size();
                assert!(size.width >= 1 && size.width <= 78, "width {}", size.width);
                assert!(size.height >= 1 && size.height <= 8, "height {}", size.height);
            }
            other => panic!("expected an image, got {other:?}"),
        }

        match load_embed(&picker, &index, "nope.png", box_size) {
            Media::Image { target, protocol, note } => {
                assert_eq!(target, "nope.png");
                assert!(protocol.is_none());
                assert_eq!(note, Some("not found"));
            }
            other => panic!("expected an image, got {other:?}"),
        }

        match load_embed(&picker, &index, "pomelo.mp3", box_size) {
            Media::Audio { target, path, failed } => {
                assert_eq!(target, "pomelo.mp3");
                assert_eq!(path.as_deref(), Some("media/pomelo.mp3"));
                assert!(!failed);
            }
            other => panic!("expected audio, got {other:?}"),
        }

        match load_embed(&picker, &index, "x.svg", box_size) {
            Media::Other { target } => assert_eq!(target, "x.svg"),
            other => panic!("expected other, got {other:?}"),
        }

        // `load_side` is that function per embed: text skipped, embed order kept.
        let segments =
            vec![embed("pomelo.png"), Segment::Text("x".to_string()), embed("pomelo.mp3")];
        let media = load_side(&picker, &index, &segments, box_size);
        assert_eq!(media.len(), 2, "{media:?}");
        assert!(
            matches!(&media[0], Media::Image { target, .. } if target == "pomelo.png"),
            "{media:?}"
        );
        assert!(
            matches!(&media[1], Media::Audio { target, .. } if target == "pomelo.mp3"),
            "{media:?}"
        );
    }

    #[test]
    fn resolve_exact_path_then_by_name_then_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = vec![
            "media/pomelo.png".to_string(),
            "other/pomelo.png".to_string(),
            "media/pomelo.mp3".to_string(),
        ];
        for rel in &paths {
            let abs = root.join(rel);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&abs, "x").unwrap();
        }
        let index = MediaIndex::new(root, paths);

        assert_eq!(index.resolve("media/pomelo.png").as_deref(), Some("media/pomelo.png"));
        assert_eq!(index.resolve("other/pomelo.png").as_deref(), Some("other/pomelo.png"));
        assert_eq!(index.resolve("pomelo.png").as_deref(), Some("media/pomelo.png"));
        assert_eq!(index.resolve("pomelo.mp3").as_deref(), Some("media/pomelo.mp3"));
        assert_eq!(index.resolve("nope.png"), None);
        assert_eq!(index.resolve("x/pomelo.png").as_deref(), Some("media/pomelo.png"));
        assert_eq!(index.abs("media/pomelo.png"), root.join("media/pomelo.png"));
    }

    #[test]
    fn null_audio_records_plays_and_stops() {
        let (mut audio, log) = NullAudio::new();

        audio.play(Path::new("a.mp3")).unwrap();
        audio.play(Path::new("a.mp3")).unwrap();
        audio.stop();

        assert_eq!(log.lock().unwrap().plays, ["a.mp3", "a.mp3"]);
        assert_eq!(log.lock().unwrap().stops, 1);

        assert!(!audio.is_playing(), "nothing is playing until the log says so");
        log.lock().unwrap().playing = true;
        assert!(audio.is_playing());
    }

    #[test]
    fn null_audio_can_fail() {
        let (mut audio, log) = NullAudio::new();
        log.lock().unwrap().fail_with = Some("no device".to_string());

        let err = audio.play(Path::new("a.mp3")).unwrap_err();

        assert_eq!(err.to_string(), "no device");
        assert!(log.lock().unwrap().plays.is_empty(), "a failed play records nothing");
    }

    /// Pins the checked-in fixture media as real, decodable files, not the
    /// zero-byte placeholders they started as. Guards against the fixtures
    /// silently regressing to empty files again.
    #[test]
    fn fixture_media_are_real_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault/media");

        let png = root.join("pomelo.png");
        let jpg = root.join("buddhas-hand.jpg");
        let mp3 = root.join("pomelo.mp3");

        assert!(fs::metadata(&png).unwrap().len() > 0, "pomelo.png is empty");
        assert!(fs::metadata(&jpg).unwrap().len() > 0, "buddhas-hand.jpg is empty");
        assert!(fs::metadata(&mp3).unwrap().len() > 0, "pomelo.mp3 is empty");

        let png_img = image::open(&png).unwrap();
        assert!(png_img.width() >= 32 && png_img.height() >= 32);

        let jpg_img = image::open(&jpg).unwrap();
        assert!(jpg_img.width() >= 32 && jpg_img.height() >= 32);

        let mp3_file = fs::File::open(&mp3).unwrap();
        rodio::Decoder::try_from(mp3_file).unwrap();
    }
}
