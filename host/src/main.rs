//! alc-cam-host: 拠点の Chrome 拡張 (extension/) が native messaging で起動する。
//! 拡張が開いている間だけ動き、stdin が閉じたら (= 拡張が閉じたら) 終わる。
//!
//!   拡張 ──stdin──▶ config / start / stop / sdp_answer / ice_candidate
//!   拡張 ◀─stdout── ready / sdp_offer / camera / peer / log
//!
//! 見る人が来たら (start) カメラの RTSP を受け始め、WebRTC の offer を返す。映像は見る側と P2P。

mod camera;
mod native;
mod peer;
mod stun;

use std::io;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use native::{In, Out};
use peer::Viewer;

fn main() {
    str0m::crypto::from_feature_flags().install_process_default();

    let (tx, rx) = mpsc::channel::<Option<Result<In, String>>>();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            match native::read(&mut stdin) {
                Ok(Some(m)) => {
                    if tx.send(Some(m)).is_err() {
                        return;
                    }
                }
                // 閉じた・壊れた: どちらも終わる
                Ok(None) | Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
            }
        }
    });

    let mut stdout = io::stdout().lock();
    let mut out = |m: Out| {
        if native::write(&mut stdout, &m).is_err() {
            std::process::exit(0); // 拡張が居ない
        }
    };
    out(Out::Ready { version: env!("CARGO_PKG_VERSION") });

    let mut config: Option<(String, String)> = None;
    let mut viewer: Option<Viewer> = None;

    loop {
        // 見る人が居ないあいだは stdin を待つだけ
        let wait = if viewer.is_some() { Duration::ZERO } else { Duration::from_millis(500) };
        let msg = match rx.recv_timeout(wait) {
            Ok(Some(m)) => Some(m),
            Ok(None) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => None,
        };

        match msg {
            None => {}
            Some(Err(e)) => out(Out::Log { message: format!("読めないメッセージ: {e}") }),
            Some(Ok(In::Config { rtsp_url, stun })) => config = Some((rtsp_url, stun)),
            Some(Ok(In::Start)) => {
                viewer = None; // 前の見る人の分は畳む
                match &config {
                    None => out(Out::Log { message: "config より先に start が来ました".into() }),
                    Some((rtsp_url, stun)) => match Viewer::new(rtsp_url, stun) {
                        Ok((v, sdp)) => {
                            viewer = Some(v);
                            out(Out::SdpOffer { sdp });
                            out(Out::Peer { ok: false, message: "offer を送信、answer 待ち".into() });
                        }
                        Err(e) => out(Out::Peer { ok: false, message: e }),
                    },
                }
            }
            Some(Ok(In::Stop)) => {
                if viewer.take().is_some() {
                    out(Out::Peer { ok: false, message: "見る人が離れました".into() });
                    out(Out::Camera { ok: false, message: "停止".into() });
                }
            }
            Some(Ok(In::SdpAnswer { sdp })) => {
                if let Some(v) = viewer.as_mut() {
                    if let Err(e) = v.accept_answer(&sdp) {
                        out(Out::Peer { ok: false, message: e });
                        viewer = None;
                    }
                }
            }
            Some(Ok(In::IceCandidate { candidate })) => {
                if let Some(v) = viewer.as_mut() {
                    v.add_candidate(&candidate);
                }
            }
        }

        if let Some(v) = viewer.as_mut() {
            match v.step(&mut out) {
                Ok(true) => {}
                Ok(false) => {
                    viewer = None;
                    out(Out::Peer { ok: false, message: "切断されました".into() });
                }
                Err(e) => {
                    viewer = None;
                    out(Out::Peer { ok: false, message: e });
                }
            }
        }
    }
}
