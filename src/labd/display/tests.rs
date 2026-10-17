//! Which capture path [`Display::grab`] takes, against a mock QMP socket and
//! the fake RFB server.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use image::RgbImage;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use super::{Display, DisplayHost};
use crate::profiles::InputTransport;
use crate::qmp::QmpClient;
use crate::vnc::fake::{self, FakeRfb, pattern, raw_rect, update};

/// How the mock QMP server answers `screendump`.
#[derive(Clone)]
enum Screendump {
    /// Write this image as a PPM to the requested file.
    Writes(RgbImage),
    /// Fail with this `GenericError` description.
    Fails(&'static str),
}

/// A mock QMP server answering `qmp_capabilities` and `screendump`, counting
/// the screendumps it was asked for.
fn spawn_qmp(dir: &std::path::Path, answer: Screendump) -> (PathBuf, Arc<AtomicUsize>) {
    let path = dir.join("qmp.sock");
    let listener = UnixListener::bind(&path).expect("bind mock qmp");
    let dumps = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&dumps);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (r, mut w) = stream.into_split();
            let mut r = BufReader::new(r);
            w.write_all(b"{\"QMP\":{\"version\":{},\"capabilities\":[]}}\n")
                .await
                .unwrap();
            let mut line = String::new();
            while r.read_line(&mut line).await.unwrap_or(0) > 0 {
                let msg: Value = serde_json::from_str(line.trim()).unwrap();
                line.clear();
                let reply = match msg["execute"].as_str() {
                    Some("screendump") => {
                        counted.fetch_add(1, Ordering::SeqCst);
                        match &answer {
                            Screendump::Writes(img) => {
                                let file = msg["arguments"]["filename"].as_str().unwrap();
                                let mut ppm =
                                    format!("P6\n{} {}\n255\n", img.width(), img.height())
                                        .into_bytes();
                                ppm.extend_from_slice(img.as_raw());
                                std::fs::write(file, ppm).unwrap();
                                json!({"return": {}, "id": msg["id"]})
                            }
                            Screendump::Fails(desc) => json!({
                                "error": {"class": "GenericError", "desc": desc},
                                "id": msg["id"],
                            }),
                        }
                    }
                    _ => json!({"return": {}, "id": msg["id"]}),
                };
                w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
            }
        }
    });
    (path, dumps)
}

struct FakeHost {
    dir: tempfile::TempDir,
    qmp: PathBuf,
    vnc: PathBuf,
    gl: bool,
    qmp_opens: Mutex<usize>,
}

#[async_trait::async_trait]
impl DisplayHost for FakeHost {
    fn name(&self) -> &str {
        "fake"
    }
    async fn qmp(&self) -> Result<QmpClient> {
        *self.qmp_opens.lock().unwrap() += 1;
        Ok(QmpClient::connect(&self.qmp).await?)
    }
    fn vnc_sock(&self) -> PathBuf {
        self.vnc.clone()
    }
    fn capture_dir(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }
    fn input_transport(&self) -> InputTransport {
        InputTransport::Qmp
    }
    fn gl_display(&self) -> bool {
        self.gl
    }
}

/// A display whose QMP answers `screendump` with `answer` and whose VNC
/// serves `vnc_img`. Returns the fake RFB server for its message log.
fn display(
    gl: bool,
    answer: Screendump,
    vnc_img: &RgbImage,
) -> (Display, Arc<FakeHost>, FakeRfb, Arc<AtomicUsize>) {
    let dir = tempfile::tempdir().unwrap();
    let (qmp, dumps) = spawn_qmp(dir.path(), answer);
    let (w, h) = (vnc_img.width() as u16, vnc_img.height() as u16);
    let rfb = fake::spawn(w, h, vec![update(&[raw_rect(vnc_img, 0, 0, w, h)])]);
    let host = Arc::new(FakeHost {
        dir,
        qmp,
        vnc: rfb.path.clone(),
        gl,
        qmp_opens: Mutex::new(0),
    });
    (Display::new(host.clone()), host, rfb, dumps)
}

/// The image QMP writes, distinct from the VNC one so a test can tell which
/// path produced its result.
fn qmp_image() -> RgbImage {
    RgbImage::from_pixel(6, 4, image::Rgb([200, 10, 10]))
}

/// A GL display is captured over VNC and never asks QMP for a screendump.
#[tokio::test]
async fn grab_reads_a_gl_display_over_vnc() {
    let vnc_img = pattern(10, 6);
    let (d, host, _rfb, dumps) = display(true, Screendump::Writes(qmp_image()), &vnc_img);

    assert_eq!(d.grab().await.unwrap(), vnc_img);
    assert_eq!(dumps.load(Ordering::SeqCst), 0);
    assert_eq!(*host.qmp_opens.lock().unwrap(), 0);
}

/// A 2D display is captured by QMP screendump and VNC is not touched.
#[tokio::test]
async fn grab_uses_qmp_screendump_for_a_2d_display() {
    let (d, _host, rfb, dumps) = display(false, Screendump::Writes(qmp_image()), &pattern(10, 6));

    assert_eq!(d.grab().await.unwrap(), qmp_image());
    assert_eq!(dumps.load(Ordering::SeqCst), 1);
    assert!(rfb.received.lock().unwrap().is_empty());
}

/// A machine not known to be GL whose screendump answers `no surface` is
/// captured over VNC instead.
#[tokio::test]
async fn grab_falls_back_to_vnc_when_qmp_has_no_surface() {
    let vnc_img = pattern(10, 6);
    let (d, _host, _rfb, dumps) = display(false, Screendump::Fails("no surface"), &vnc_img);

    assert_eq!(d.grab().await.unwrap(), vnc_img);
    assert_eq!(dumps.load(Ordering::SeqCst), 1);
}

/// When the VNC fallback fails as well, the QMP error is what the caller
/// sees, with the VNC failure beside it.
#[tokio::test]
async fn grab_reports_no_surface_when_the_vnc_fallback_fails_too() {
    let dir = tempfile::tempdir().unwrap();
    let (qmp, _dumps) = spawn_qmp(dir.path(), Screendump::Fails("no surface"));
    let vnc = dir.path().join("nobody-listens.sock");
    let d = Display::new(Arc::new(FakeHost {
        dir,
        qmp,
        vnc,
        gl: false,
        qmp_opens: Mutex::new(0),
    }));

    let err = format!("{:#}", d.grab().await.unwrap_err());
    assert!(err.contains("no surface"), "{err}");
    assert!(err.contains("VNC capture fallback failed too"), "{err}");
}

/// Any other screendump failure is returned as-is, without a VNC attempt.
#[tokio::test]
async fn grab_does_not_fall_back_on_other_qmp_errors() {
    let (d, _host, rfb, _dumps) = display(false, Screendump::Fails("device busy"), &pattern(4, 4));

    let err = format!("{:#}", d.grab().await.unwrap_err());
    assert!(err.contains("device busy"), "{err}");
    assert!(rfb.received.lock().unwrap().is_empty());
}
