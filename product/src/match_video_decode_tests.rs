//! Real decoder/worker boundary proof. Root builds facial-cli before this target.
#![cfg(windows)]
use crate::{
    match_video::VideoTime,
    match_video_decode::FIRST_VIDEO_STREAM,
    match_worker::{IsolatedMatchWorker, WorkerFence},
};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    worker: Option<IsolatedMatchWorker>,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "facial-video-decoder-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&root).unwrap();
        Self {
            root: root.canonicalize().unwrap(),
            worker: None,
        }
    }
    fn generate(&self, name: &str, arguments: &[&str]) -> PathBuf {
        let executable = crate::media_thumbs::resolve_ffmpeg()
            .expect("configured FFmpeg is required for decoder boundary proof");
        let path = self.root.join(name);
        let mut args: Vec<OsString> = [
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-filter_complex_threads",
            "1",
        ]
        .into_iter()
        .chain(arguments.iter().copied())
        .map(OsString::from)
        .collect();
        args.push(path.as_os_str().to_owned());
        let (success, stdout, stderr) =
            crate::match_decoder_process::run(&executable, &args, 1024, 64 * 1024).unwrap();
        assert!(
            success,
            "fixture encoder failed: {}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(stdout.is_empty());
        assert!(std::fs::metadata(&path).unwrap().len() < 1024 * 1024);
        path
    }
    fn pin(&mut self, path: &Path) -> WorkerFence {
        let mut fence = WorkerFence {
            job_id: "decoder-job".into(),
            asset_id: "decoder-asset".into(),
            media_key: "decoder-media".into(),
            schema_generation: "decoder-schema".into(),
            identity_revision: 1,
            catalog_revision: 1,
            admission_epoch: 0,
            model_generation: "decoder-no-model".into(),
            media_fingerprint: String::new(),
            track_id: None,
            timestamp_ms: None,
        };
        let worker = self.worker.as_mut().unwrap();
        worker
            .begin_source(path, &self.root, &fence)
            .unwrap_or_else(|error| panic!("BeginSource {}: {error:?}", path.display()));
        loop {
            let progress = worker
                .hash_source_step(&fence)
                .unwrap_or_else(|error| panic!("FingerprintStep {}: {error:?}", path.display()));
            if let Some(hash) = progress.fingerprint {
                assert_eq!(
                    hash,
                    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
                );
                assert_eq!(progress.final_path, path.canonicalize().unwrap());
                assert_eq!(progress.bytes_hashed, progress.size);
                fence.media_fingerprint = hash;
                return fence;
            }
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Dropping the owned worker kills its Job, then Windows releases its
        // pinned file handle. Retry only that asynchronous sharing transition.
        drop(self.worker.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::remove_dir_all(&self.root) {
                Ok(()) => break,
                Err(error)
                    if matches!(error.raw_os_error(), Some(32 | 33))
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => {
                    if !std::thread::panicking() {
                        panic!("owned decoder fixture cleanup failed: {error}");
                    }
                    break;
                }
            }
        }
    }
}

fn same_time(time: VideoTime, numerator: i64, denominator: i64) {
    assert_eq!(
        i128::from(time.pts) * i128::from(time.numerator) * i128::from(denominator),
        i128::from(numerator) * i128::from(time.denominator)
    );
}

#[test]
fn wp086_real_decoder_cfr_vfr_audio_origin_exact_replay_and_source_pin() {
    let mut fixture = Fixture::new();
    let cfr = fixture.generate(
        "cfr.nut",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=32x32:rate=4:duration=1",
            "-map",
            "0:v:0",
            "-frames:v",
            "4",
            "-c:v",
            "ffv1",
            "-threads",
            "1",
            "-fps_mode",
            "passthrough",
            "-f",
            "nut",
        ],
    );
    let vfr = fixture.generate("audio-first-vfr-origin.nut", &[
        "-f", "lavfi", "-i", "anullsrc=r=8000:cl=mono:d=1",
        "-f", "lavfi", "-i", "testsrc2=size=32x32:rate=10:duration=0.3",
        "-filter_complex", "[0:a]asetpts=PTS+5/TB[a];[1:v]settb=1/1000,setpts=10000+if(eq(N\\,0)\\,0\\,if(eq(N\\,1)\\,200\\,700))[v]",
        "-map", "[a]", "-map", "[v]", "-copyts", "-c:a", "pcm_s16le", "-c:v", "ffv1",
        "-threads", "1", "-enc_time_base:v", "1:1000", "-fps_mode", "passthrough", "-f", "nut",
    ]);
    let sub_ms = fixture.generate(
        "sub-millisecond.nut",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=32x32:rate=2000:duration=0.002",
            "-map",
            "0:v:0",
            "-frames:v",
            "4",
            "-c:v",
            "ffv1",
            "-threads",
            "1",
            "-enc_time_base:v",
            "1:1000000",
            "-fps_mode",
            "passthrough",
            "-f",
            "nut",
        ],
    );
    let executable = crate::media_thumbs::resolve_ffmpeg().unwrap();
    let mut overflow_args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-threads",
        "1",
        "-i",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    overflow_args.push(cfr.as_os_str().to_owned());
    overflow_args.extend(
        [
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-threads",
            "1",
            "-c:v",
            "ppm",
            "-f",
            "image2pipe",
            "pipe:1",
        ]
        .into_iter()
        .map(OsString::from),
    );
    let overflow =
        crate::match_decoder_process::run(&executable, &overflow_args, 1, 64 * 1024).unwrap_err();
    assert!(
        overflow.contains("output exceeded bound"),
        "real decoder pipe bound: {overflow}"
    );
    fixture.worker =
        Some(IsolatedMatchWorker::spawn().expect("build facial-cli before decoder worker proof"));

    let fence = fixture.pin(&cfr);
    let worker = fixture.worker.as_mut().unwrap();
    let first = worker
        .decode(&cfr, 0, FIRST_VIDEO_STREAM, &fence)
        .unwrap_or_else(|error| {
            panic!(
                "Decode {} requested={:?} stream={:?}: {error:?}",
                cfr.display(),
                0,
                FIRST_VIDEO_STREAM
            )
        })
        .unwrap();
    assert_eq!(first.stream_index, 0);
    same_time(first.time, 0, 1);
    same_time(first.playback_origin, 0, 1);
    assert_eq!(first.scene_probe.len(), 1024);
    let second = worker
        .decode(&cfr, 250, 0, &fence)
        .unwrap_or_else(|error| {
            panic!(
                "Decode {} requested={:?} stream={:?}: {error:?}",
                cfr.display(),
                250,
                0
            )
        })
        .unwrap();
    same_time(second.time, 1, 4);
    let replay = worker
        .decode_exact(&cfr, second.time, 0, &fence)
        .unwrap_or_else(|error| {
            panic!(
                "DecodeExact {} requested={:?} stream={:?}: {error:?}",
                cfr.display(),
                second.time,
                0
            )
        })
        .unwrap();
    assert_eq!(second.frame_sha256, replay.frame_sha256);
    assert_eq!(second.encoded, replay.encoded);
    assert!(worker
        .decode(&cfr, 2000, 0, &fence)
        .unwrap_or_else(|error| panic!(
            "Decode {} requested={:?} stream={:?}: {error:?}",
            cfr.display(),
            2000,
            0
        ))
        .is_none());
    assert!(
        worker.decode(&vfr, 0, FIRST_VIDEO_STREAM, &fence).is_err(),
        "unpinned path must fail"
    );
    let write = std::fs::OpenOptions::new().write(true).open(&cfr);
    assert!(
        write.is_err(),
        "live worker pin must reject modifying source bytes"
    );
    assert_eq!(write.unwrap_err().raw_os_error(), Some(32));

    let fence = fixture.pin(&vfr);
    let worker = fixture.worker.as_mut().unwrap();
    let first = worker
        .decode(&vfr, 0, FIRST_VIDEO_STREAM, &fence)
        .unwrap_or_else(|error| {
            panic!(
                "Decode {} requested={:?} stream={:?}: {error:?}",
                vfr.display(),
                0,
                FIRST_VIDEO_STREAM
            )
        })
        .unwrap();
    assert_eq!(first.stream_index, 1, "audio stream precedes video");
    same_time(first.time, 10, 1);
    same_time(first.playback_origin, 5, 1);
    assert_eq!(
        first
            .time
            .playback_milliseconds(first.playback_origin)
            .unwrap(),
        5000
    );
    let second = worker
        .decode(&vfr, 10100, 1, &fence)
        .unwrap_or_else(|error| {
            panic!(
                "Decode {} requested={:?} stream={:?}: {error:?}",
                vfr.display(),
                10100,
                1
            )
        })
        .unwrap();
    same_time(second.time, 102, 10);
    let third = worker
        .decode(&vfr, 10300, 1, &fence)
        .unwrap_or_else(|error| {
            panic!(
                "Decode {} requested={:?} stream={:?}: {error:?}",
                vfr.display(),
                10300,
                1
            )
        })
        .unwrap();
    same_time(third.time, 107, 10);
    assert_eq!(
        worker
            .decode_exact(&vfr, third.time, 1, &fence)
            .unwrap_or_else(|error| panic!(
                "DecodeExact {} requested={:?} stream={:?}: {error:?}",
                vfr.display(),
                third.time,
                1
            ))
            .unwrap()
            .frame_sha256,
        third.frame_sha256
    );

    let fence = fixture.pin(&sub_ms);
    let worker = fixture.worker.as_mut().unwrap();
    let first = worker
        .decode(&sub_ms, 0, 0, &fence)
        .unwrap_or_else(|error| {
            panic!(
                "Decode {} requested={:?} stream={:?}: {error:?}",
                sub_ms.display(),
                0,
                0
            )
        })
        .unwrap();
    assert_eq!(
        (first.width, first.height),
        (32, 32),
        "small source frames must not be enlarged for transport"
    );
    assert!(
        first.encoded.len() < 4096,
        "native-size PPM stays within its source pixel budget"
    );
    // Use the decoder's own stream timebase; 0.5 ms must remain distinct from 0.
    assert_eq!(first.time.numerator, 1);
    assert_eq!(first.time.denominator % 2000, 0);
    let exact = VideoTime {
        pts: i64::from(first.time.denominator / 2000),
        numerator: first.time.numerator,
        denominator: first.time.denominator,
    };
    same_time(exact, 1, 2000);
    let second = worker
        .decode_exact(&sub_ms, exact, 0, &fence)
        .unwrap_or_else(|error| {
            panic!(
                "DecodeExact {} requested={:?} stream={:?}: {error:?}",
                sub_ms.display(),
                exact,
                0
            )
        })
        .unwrap();
    assert_eq!((second.width, second.height), (32, 32));
    assert_eq!(second.time.milliseconds().unwrap(), 0);
    assert_ne!(second.time, first.time);
    assert_eq!(
        worker
            .decode_exact(&sub_ms, second.time, 0, &fence)
            .unwrap_or_else(|error| panic!(
                "DecodeExact {} requested={:?} stream={:?}: {error:?}",
                sub_ms.display(),
                second.time,
                0
            ))
            .unwrap()
            .frame_sha256,
        second.frame_sha256
    );
}
