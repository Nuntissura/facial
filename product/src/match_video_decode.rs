//! Exact frame extraction, called only inside the supervised Match worker.
//! FFmpeg inherits that worker's Job; the supervisor bounds the complete call.
use crate::match_video::VideoTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{ffi::OsString, io::Read, path::Path};

const EDGE: u32 = 640;
const IMAGE_LIMIT: usize = (EDGE * EDGE * 3) as usize + 4096;
const LOG_LIMIT: usize = 256 * 1024;
pub(crate) const FIRST_VIDEO_STREAM: u32 = u32::MAX;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VideoSourceProgress {
    pub final_path: std::path::PathBuf,
    pub size: u64,
    pub bytes_hashed: u64,
    pub fingerprint: Option<String>,
}

/// Private worker protocol; these values never enter API receipts or UI JSON.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaybackSourceHandles {
    pub final_path: std::path::PathBuf,
    pub handles: Vec<u64>,
}

pub(crate) struct PlaybackSourcePin {
    pub final_path: std::path::PathBuf,
    pub(crate) _files: Vec<std::fs::File>,
}

/// A live read-sharing-only handle prevents source replacement or modification
/// while the child hashes and decodes it. No compiler or foreground pool owns it.
pub(crate) struct VideoSourceReader {
    file: std::fs::File,
    directory_pins: Vec<std::fs::File>,
    progress: VideoSourceProgress,
    hash: Sha256,
}
impl VideoSourceReader {
    pub(crate) fn begin(path: &Path, expected_root: &Path) -> Result<Self, String> {
        let expected_root = expected_root
            .canonicalize()
            .map_err(|_| "video root unavailable")?;
        let expected = path
            .canonicalize()
            .map_err(|_| "video source unavailable")?;
        if !expected.starts_with(&expected_root) {
            return Err("video source escaped root".into());
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1); // FILE_SHARE_READ; deny concurrent writes/replacement.
        }
        let file = options
            .open(&expected)
            .map_err(|_| "video source cannot be pinned for reading")?;
        let actual = crate::identity::opened_file_final_path(&file, "match_video")?;
        let metadata = file
            .metadata()
            .map_err(|_| "video source metadata unavailable")?;
        if actual != expected
            || !actual.starts_with(&expected_root)
            || !metadata.is_file()
            || metadata.len() > 64 * 1024 * 1024 * 1024
        {
            return Err("video source identity or 64 GiB size bound rejected".into());
        }
        Ok(Self {
            file,
            directory_pins: Vec::new(),
            hash: Sha256::new(),
            progress: VideoSourceProgress {
                final_path: actual,
                size: metadata.len(),
                bytes_hashed: 0,
                fingerprint: None,
            },
        })
    }
    #[cfg(windows)]
    pub(crate) fn playback_handles(&mut self) -> Result<PlaybackSourceHandles, String> {
        use std::os::windows::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawHandle,
        };
        let directories = self
            .progress
            .final_path
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
            .collect::<Vec<_>>();
        if directories.len() > 64 {
            return Err("playback namespace exceeds 64 directory pins".into());
        }
        for path in directories {
            let mut options = std::fs::OpenOptions::new();
            // Attribute-only access permits ordinary sibling file activity; denying
            // delete sharing prevents namespace replacement while the source is active.
            options
                .access_mode(0x80)
                .share_mode(3)
                .custom_flags(0x02000000 | 0x00200000);
            let directory = options
                .open(path)
                .map_err(|_| "playback directory cannot be pinned")?;
            let metadata = directory
                .metadata()
                .map_err(|_| "playback directory identity unavailable")?;
            if !metadata.is_dir()
                || metadata.file_attributes() & 0x400 != 0
                || crate::identity::opened_file_final_path(&directory, "playback_directory")?
                    != path
            {
                return Err(
                    "playback namespace changed or contains unresolved reparse point".into(),
                );
            }
            self.directory_pins.push(directory);
        }
        if crate::identity::opened_file_final_path(&self.file, "playback_source")?
            != self.progress.final_path
        {
            return Err("playback source namespace changed".into());
        }
        let handles = std::iter::once(&self.file)
            .chain(self.directory_pins.iter())
            .map(|file| file.as_raw_handle() as usize as u64)
            .collect();
        Ok(PlaybackSourceHandles {
            final_path: self.progress.final_path.clone(),
            handles,
        })
    }
    #[cfg(not(windows))]
    pub(crate) fn playback_handles(&mut self) -> Result<PlaybackSourceHandles, String> {
        Err("native playback pinning requires Windows".into())
    }

    pub(crate) fn step(&mut self) -> Result<VideoSourceProgress, String> {
        if self.progress.fingerprint.is_some() {
            return Ok(self.progress.clone());
        }
        let mut buffer = vec![0u8; 4 * 1024 * 1024];
        let count = self
            .file
            .read(&mut buffer)
            .map_err(|_| "video source read failed")?;
        self.hash.update(&buffer[..count]);
        self.progress.bytes_hashed = self
            .progress
            .bytes_hashed
            .checked_add(count as u64)
            .ok_or("video source length overflow")?;
        if self.progress.bytes_hashed > self.progress.size {
            return Err("video source grew during hash".into());
        }
        if count == 0 {
            if self.progress.bytes_hashed != self.progress.size {
                return Err("video source truncated during hash".into());
            }
            self.progress.fingerprint = Some(format!("{:x}", self.hash.clone().finalize()));
        }
        Ok(self.progress.clone())
    }
    pub(crate) fn matches(&self, path: &Path) -> bool {
        self.progress.fingerprint.is_some() && path == self.progress.final_path
    }
    pub(crate) fn image_bytes(&self, path: &Path, fingerprint: &str) -> Result<Vec<u8>, String> {
        use std::io::{Seek, SeekFrom};
        const LIMIT: u64 = 256 * 1024 * 1024;
        if !self.matches(path)
            || self.progress.size > LIMIT
            || crate::match_store::canonical_media_sha256(fingerprint)
                != self.progress.fingerprint.as_deref()
        {
            return Err("image source path, fingerprint or 256 MiB bound rejected".into());
        }
        let mut reader = self
            .file
            .try_clone()
            .map_err(|_| "pinned image handle unavailable")?;
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|_| "pinned image rewind failed")?;
        let mut bytes = Vec::with_capacity(self.progress.size as usize);
        reader
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "pinned image read failed")?;
        if bytes.len() as u64 != self.progress.size
            || format!("{:x}", Sha256::digest(&bytes))
                != self.progress.fingerprint.as_deref().unwrap_or("")
            || crate::identity::opened_file_final_path(&self.file, "match_image")?
                != self.progress.final_path
        {
            return Err("pinned image bytes or handle identity changed".into());
        }
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceContainer {
    Matroska,
    IsoBmff,
    Unsupported,
}

fn parse_container(log: &[u8]) -> Result<SourceContainer, String> {
    let text = std::str::from_utf8(log).map_err(|_| "invalid source container header")?;
    let mut headers = text
        .lines()
        .filter_map(|line| line.strip_prefix("Input #0, "));
    let header = headers.next().ok_or("source container header missing")?;
    if headers.next().is_some() {
        return Err("ambiguous source container header".into());
    }
    let (format, _) = header
        .split_once(", from ")
        .ok_or("invalid source container header")?;
    Ok(match format {
        "matroska,webm" => SourceContainer::Matroska,
        "mov,mp4,m4a,3gp,3g2,mj2" => SourceContainer::IsoBmff,
        _ => SourceContainer::Unsupported,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecodedVideoSample {
    pub container: SourceContainer,
    pub stream_index: u32,
    pub time: VideoTime,
    pub playback_origin: VideoTime,
    pub frame_sha256: String,
    pub width: u32,
    pub height: u32,
    pub encoded: Vec<u8>,
    pub scene_probe: Vec<u8>,
}

fn bounded_read(mut input: impl Read, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "video decoder pipe failed".to_string())?;
    if bytes.len() > limit {
        return Err("video decoder output exceeded bound".into());
    }
    Ok(bytes)
}

/// `None` means successful EOF. Requested seek is never used as frame provenance.
pub(crate) fn decode_sample(
    path: &Path,
    requested_ms: u64,
    stream_index: u32,
) -> Result<Option<DecodedVideoSample>, String> {
    decode_impl(path, requested_ms, stream_index, None)
}

pub(crate) fn decode_exact_sample(
    path: &Path,
    time: VideoTime,
    stream_index: u32,
) -> Result<Option<DecodedVideoSample>, String> {
    time.validate()?;
    decode_impl(path, time.milliseconds()?, stream_index, Some(time))
}

fn decode_impl(
    path: &Path,
    requested_ms: u64,
    stream_index: u32,
    exact: Option<VideoTime>,
) -> Result<Option<DecodedVideoSample>, String> {
    if requested_ms > 7 * 24 * 60 * 60 * 1000
        || (stream_index > 1024 && stream_index != FIRST_VIDEO_STREAM)
    {
        return Err("video sample request exceeds bounds".into());
    }
    let executable = crate::media_thumbs::resolve_ffmpeg()
        .ok_or("video decoder unavailable: configure FACIAL_FFMPEG")?;
    let before = std::fs::metadata(path).map_err(|_| "video source unavailable")?;
    if !before.is_file() {
        return Err("video source is not a file".into());
    }
    let seek = format!("{}.{:03}", requested_ms / 1000, requested_ms % 1000);
    let mapping = if stream_index == FIRST_VIDEO_STREAM {
        "0:v:0".to_string()
    } else {
        format!("0:{stream_index}")
    };
    // Preserve input timestamps, seek in absolute stream time, select the exact
    // stream, and prevent filter lookahead from producing a second metadata row.
    // Showinfo precedes scaling: its PTS/timebase belongs to the source frame.
    // Keep pre-roll and select by original timestamps ourselves: FFmpeg's
    // automatic seek discard can exhaust streams with different start offsets.
    let select = exact
        .map(|time| format!("select=gte(pts\\,{}),", time.pts))
        .unwrap_or_else(|| format!("select=gte(t\\,{seek}),"));
    let filter = format!("{select}trim=end_frame=1,showinfo,scale=w=min(iw\\,{EDGE}):h=min(ih\\,{EDGE}):force_original_aspect_ratio=decrease:flags=bilinear");
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-loglevel",
        "info",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-max_pixels",
        "8294400",
        "-copyts",
        "-noaccurate_seek",
        "-seek_timestamp",
        "1",
        "-ss",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.extend([
        OsString::from(seek),
        OsString::from("-i"),
        path.as_os_str().to_owned(),
    ]);
    args.extend(
        [
            "-map",
            &mapping,
            "-an",
            "-sn",
            "-dn",
            "-vf",
            &filter,
            "-frames:v",
            "1",
            "-fps_mode",
            "passthrough",
            "-c:v",
            "ppm",
            "-pix_fmt",
            "rgb24",
            "-threads",
            "1",
            "-f",
            "image2pipe",
            "pipe:1",
        ]
        .into_iter()
        .map(OsString::from),
    );
    let (success, encoded, log) =
        crate::match_decoder_process::run(&executable, &args, IMAGE_LIMIT, LOG_LIMIT)?;
    if !success {
        return Err("video decoder failed or exceeded memory bound".into());
    }
    let after = std::fs::metadata(path).map_err(|_| "video source disappeared")?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err("video source changed during sample".into());
    }
    let time = parse_time(&log)?;
    if encoded.is_empty() && time.is_none() {
        return Ok(None);
    }
    let time = time.ok_or("video decoder omitted exact frame timestamp")?;
    if exact.is_some_and(|expected| expected != time) {
        return Err("video exact PTS/timebase mismatch".into());
    }
    let playback_origin = parse_origin(&log)?;
    let actual_stream = parse_stream(&log)?;
    if stream_index != FIRST_VIDEO_STREAM && actual_stream != stream_index {
        return Err("video decoder stream mismatch".into());
    }
    let image = image::load_from_memory_with_format(&encoded, image::ImageFormat::Pnm)
        .map_err(|_| "video decoder returned invalid frame")?;
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 || width > EDGE || height > EDGE {
        return Err("video decoder frame exceeds bounds".into());
    }
    let scene_probe = image
        .resize_exact(32, 32, image::imageops::FilterType::Triangle)
        .to_luma8()
        .into_raw();
    Ok(Some(DecodedVideoSample {
        container: parse_container(&log)?,
        stream_index: actual_stream,
        time,
        playback_origin,
        frame_sha256: format!("{:x}", Sha256::digest(&encoded)),
        width,
        height,
        encoded,
        scene_probe,
    }))
}

fn parse_origin(log: &[u8]) -> Result<VideoTime, String> {
    let text = std::str::from_utf8(log).map_err(|_| "invalid container timeline")?;
    let mut origins = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Duration: "))
        .filter_map(|line| line.split("start: ").nth(1))
        .filter_map(|line| line.split(',').next());
    let raw = origins
        .next()
        .ok_or("container timeline origin missing")?
        .trim();
    if raw.starts_with('-') {
        return Err("negative container timeline origin is unsupported".into());
    }
    if origins.next().is_some() {
        return Err("ambiguous container timeline origin".into());
    }
    let (seconds, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if fraction.len() > 6 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid container timeline origin".into());
    }
    let seconds = seconds
        .parse::<i64>()
        .map_err(|_| "invalid container timeline origin")?;
    let fraction = format!("{fraction:0<6}")
        .parse::<i64>()
        .map_err(|_| "invalid container timeline origin")?;
    let pts = seconds
        .checked_mul(1_000_000)
        .and_then(|s| s.checked_add(fraction))
        .ok_or("container timeline overflow")?;
    let origin = VideoTime {
        pts,
        numerator: 1,
        denominator: 1_000_000,
    };
    origin.validate()?;
    Ok(origin)
}

fn parse_stream(log: &[u8]) -> Result<u32, String> {
    let text = std::str::from_utf8(log).map_err(|_| "invalid video mapping")?;
    let mut streams = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Stream #0:"))
        .filter_map(|line| line.split_once(" -> #0:0 ").map(|(index, _)| index))
        .map(|index| {
            index
                .parse::<u32>()
                .map_err(|_| "invalid video source stream")
        });
    let stream = streams
        .next()
        .ok_or("video source stream mapping missing")??;
    if streams.next().is_some() || stream > 1024 {
        return Err("ambiguous video source stream".into());
    }
    Ok(stream)
}

fn parse_time(log: &[u8]) -> Result<Option<VideoTime>, String> {
    let text = std::str::from_utf8(log).map_err(|_| "video timestamp output is invalid UTF-8")?;
    let mut base = None;
    let mut pts = None;
    for line in text
        .lines()
        .filter(|line| line.starts_with("[Parsed_showinfo_"))
    {
        if let Some(value) = line.split("config in time_base: ").nth(1) {
            let value = value.split(',').next().unwrap_or("");
            let (n, d) = value.split_once('/').ok_or("video timebase missing")?;
            let next = (
                n.trim()
                    .parse::<u32>()
                    .map_err(|_| "invalid video timebase")?,
                d.trim()
                    .parse::<u32>()
                    .map_err(|_| "invalid video timebase")?,
            );
            if base.replace(next).is_some() {
                return Err("ambiguous video timebase".into());
            }
        }
        if let Some(value) = line.split(" n:").nth(1) {
            let (index, rest) = value
                .split_once(" pts:")
                .ok_or("video frame timestamp missing")?;
            if index.trim() != "0" || pts.is_some() {
                return Err("ambiguous video frame timestamp".into());
            }
            let raw = rest.split_whitespace().next().ok_or("video PTS missing")?;
            pts = Some(raw.parse::<i64>().map_err(|_| "invalid video PTS")?);
        }
    }
    let Some(pts) = pts else {
        return Ok(None);
    };
    let (numerator, denominator) = base.ok_or("video timebase missing")?;
    let time = VideoTime {
        pts,
        numerator,
        denominator,
    };
    time.validate()?;
    Ok(Some(time))
}

pub(crate) fn scene_change(previous: &[u8], next: &[u8]) -> Result<f32, String> {
    if previous.len() != 1024 || next.len() != 1024 {
        return Err("invalid scene probe".into());
    }
    Ok(previous
        .iter()
        .zip(next)
        .map(|(a, b)| u32::from(a.abs_diff(*b)))
        .sum::<u32>() as f32
        / (1024.0 * 255.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_rational_pts_is_preserved_without_seek_substitution() {
        let log = b"[Parsed_showinfo_1 @ 0] config in time_base: 1/90000, frame_rate: 30000/1001\n[Parsed_showinfo_1 @ 0] n:   0 pts:  93093 pts_time:1.03437 fmt:rgb24\n";
        let time = parse_time(log).unwrap().unwrap();
        assert_eq!(
            time,
            VideoTime {
                pts: 93093,
                numerator: 1,
                denominator: 90000
            }
        );
        assert_eq!(time.milliseconds().unwrap(), 1034);
        assert!(parse_time(&[log.as_slice(), log.as_slice()].concat()).is_err());
    }
    #[test]
    fn missing_invalid_and_multiple_timestamp_evidence_fails_closed() {
        assert_eq!(parse_time(b"EOF\n").unwrap(), None);
        assert!(parse_time(b"[Parsed_showinfo_1 @ 0] n: 0 pts: 10\n").is_err());
        assert!(parse_time(b"[Parsed_showinfo_1 @ 0] config in time_base: 1/0,\n[Parsed_showinfo_1 @ 0] n: 0 pts: 10\n").is_err());
        assert!(parse_time(b"[Parsed_showinfo_1 @ 0] config in time_base: 1/1,\n[Parsed_showinfo_1 @ 0] n: 1 pts: 10\n").is_err());
    }
    #[test]
    fn scene_probe_and_pipe_memory_are_bounded() {
        assert_eq!(scene_change(&[0; 1024], &[255; 1024]).unwrap(), 1.0);
        assert_eq!(scene_change(&[90; 1024], &[90; 1024]).unwrap(), 0.0);
        assert!(scene_change(&[], &[]).is_err());
        assert!(bounded_read(&[0; 5][..], 4).is_err());
        assert_eq!(
            parse_stream(b"Stream mapping:\n  Stream #0:1 -> #0:0 (ffv1 -> ppm)\n").unwrap(),
            1
        );
        assert!(parse_stream(b"Stream mapping:\n").is_err());
        assert_eq!(
            parse_origin(b"  Duration: 00:00:11.00, start: 0.000000, bitrate: 500 kb/s\n")
                .unwrap()
                .pts,
            0
        );
        assert_eq!(
            parse_origin(b"  Duration: 00:00:11.00, start: 10.123456, bitrate: 500 kb/s\n")
                .unwrap()
                .pts,
            10123456
        );
    }
}

#[cfg(test)]
mod container_tests {
    use super::*;
    #[test]
    fn wp086_native_container_provenance_rejects_ambiguity_and_never_uses_extension() {
        assert_eq!(
            parse_container(b"Input #0, matroska,webm, from 'misleading.mp4':\n").unwrap(),
            SourceContainer::Matroska
        );
        assert_eq!(
            parse_container(b"Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'misleading.mkv':\n")
                .unwrap(),
            SourceContainer::IsoBmff
        );
        assert_eq!(
            parse_container(b"Input #0, avi, from 'looks.mkv':\n").unwrap(),
            SourceContainer::Unsupported
        );
        assert!(
            parse_container(b"Input #0, avi, from 'a':\nInput #0, matroska,webm, from 'b':\n")
                .is_err()
        );
        assert!(parse_container(b"Output #0, matroska,webm, to 'a':\n").is_err());
    }
}
