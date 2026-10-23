//! alc-cam-host: 拠点の Chrome 拡張 (extension/) が native messaging で起動する。
//! 拡張が開いている間だけ動き、stdin が閉じたら (= 拡張が閉じたら) 終わる。
//!
//!   拡張 ──stdin──▶ config / start / stop / sdp_answer / ice_candidate   (config 以外は viewer_id 付き)
//!   拡張 ◀─stdout── ready / sdp_offer / camera / peer / log
//!
//! 首振りは映像と同じ PeerConnection のデータチャネル `ptz` で見る側から直接届き、カメラの管理画面 (HTTP) へ送る
//! (`ptz`。拡張は関わらない)。
//!
//! 見る人が来るたび (start) その人への WebRTC の offer を返す。カメラの RTSP は最初の 1 人で開き、
//! 最後の 1 人が離れたら TEARDOWN する (カメラにつき 1 本)。映像は `hub` が見る人ごとに複製して P2P で送る。

mod camera;
mod hub;
mod native;
mod peer;
mod ptz;
mod stun;

use std::io;
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use camera::{Cam, Camera};
use hub::{Frame, Hub, MAX_VIEWERS};
use native::{In, Out};
use peer::Viewer;
use ptz::Ptz;

/// 1 巡で UDP を待つ時間の合計 (見る人で割る)。stdin とカメラのフレームにもすぐ応じるため短く
const MAX_WAIT: Duration = Duration::from_millis(5);

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
    let mut hub: Hub<Viewer> = Hub::default();
    let mut camera: Option<Camera> = None;
    let mut ptz: Option<Ptz> = None;
    let mut ptz_ctl = cam_ptz::Controller::default();

    loop {
        // 見る人が居ないあいだは stdin を待つだけ
        let wait = if hub.is_empty() { Duration::from_millis(500) } else { Duration::ZERO };
        let msg = match rx.recv_timeout(wait) {
            Ok(Some(m)) => Some(m),
            Ok(None) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => None,
        };

        match msg {
            None => {}
            Some(Err(e)) => out(Out::Log { message: format!("読めないメッセージ: {e}") }),
            Some(Ok(In::Config { rtsp_url, stun })) => {
                ptz = Ptz::open(&rtsp_url).map_err(|e| out(Out::Log { message: format!("首振りは使えません: {e}") })).ok();
                ptz_ctl = cam_ptz::Controller::default();
                config = Some((rtsp_url, stun));
            }
            Some(Ok(In::Start { viewer_id })) => match &config {
                None => out(Out::Log { message: "config より先に start が来ました".into() }),
                Some(_) if !hub.contains(&viewer_id) && hub.len() >= MAX_VIEWERS => {
                    out(Out::Log { message: format!("見る人が上限 ({MAX_VIEWERS}) なので {viewer_id} は断ります") })
                }
                Some((rtsp_url, stun)) => match Viewer::new(&viewer_id, rtsp_url, stun) {
                    Ok((v, sdp)) => {
                        // 同じ viewer_id の入り直しなら前の分は畳まれる
                        let _ = hub.insert(&viewer_id, v);
                        out(Out::SdpOffer { viewer_id: viewer_id.clone(), sdp });
                        out(Out::Peer { viewer_id, ok: false, message: "offer を送信、answer 待ち".into() });
                    }
                    Err(message) => out(Out::Peer { viewer_id, ok: false, message }),
                },
            },
            Some(Ok(In::Stop { viewer_id })) => {
                ptz_ctl.forget(&viewer_id);
                if hub.remove(&viewer_id).is_some() {
                    out(Out::Peer { viewer_id, ok: false, message: "見る人が離れました".into() });
                }
            }
            Some(Ok(In::SdpAnswer { viewer_id, sdp })) => match hub.get_mut(&viewer_id) {
                None => out(Out::Log { message: format!("知らない viewer_id の answer: {viewer_id}") }),
                Some(v) => {
                    if let Err(message) = v.accept_answer(&sdp) {
                        hub.remove(&viewer_id);
                        out(Out::Peer { viewer_id, ok: false, message });
                    }
                }
            },
            Some(Ok(In::IceCandidate { viewer_id, candidate })) => match hub.get_mut(&viewer_id) {
                None => out(Out::Log { message: format!("知らない viewer_id の候補: {viewer_id}") }),
                Some(v) => v.add_candidate(&candidate),
            },
        }

        // カメラ → 全員へ複製
        if let Some(cam) = &camera {
            loop {
                match cam.rx.try_recv() {
                    Ok(Cam::Status { ok, message }) => out(Out::Camera { ok, message }),
                    Ok(Cam::Frame { key, timestamp_us, data }) => {
                        let f = Frame { key, ts: (timestamp_us * 0.09) as u64, data: data.into() };
                        for (viewer_id, _, message) in hub.on_frame(f) {
                            out(Out::Peer { viewer_id, ok: false, message });
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    // 受信スレッドが居なくなった。下で開き直す
                    Err(TryRecvError::Disconnected) => {
                        camera = None;
                        break;
                    }
                }
            }
        }

        // 見る人ごとの WebRTC。1 人の失敗はその人だけ外す
        if !hub.is_empty() {
            let per = MAX_WAIT / hub.len() as u32;
            for viewer_id in hub.ids() {
                let Some(v) = hub.get_mut(&viewer_id) else { continue };
                let message = match v.step(per, &mut out) {
                    Ok(true) => continue,
                    Ok(false) => "切断されました".to_string(),
                    Err(e) => e,
                };
                hub.remove(&viewer_id);
                out(Out::Peer { viewer_id, ok: false, message });
            }
            for (viewer_id, _, message) in hub.flush_connected() {
                out(Out::Peer { viewer_id, ok: false, message });
            }
        }

        // 首振り: 見る側の命令 → 交通整理 → カメラへ 1 本ずつ → 結果を命令した人へ
        let now = Instant::now();
        let mut replies = Vec::new();
        for viewer_id in hub.ids() {
            let Some(v) = hub.get_mut(&viewer_id) else { continue };
            for text in v.take_ptz() {
                match ptz::parse(&text) {
                    None => {}
                    Some((action, Err(message))) => v.send_ptz(&ptz::reply(&action, false, false, &message)),
                    Some((action, Ok(_))) if ptz.is_none() => v.send_ptz(&ptz::reply(&action, false, true, "首振りは使えません")),
                    Some((_, Ok(cmd))) => replies.extend(ptz_ctl.command(&viewer_id, cmd, now)),
                }
            }
        }
        if let Some(p) = &ptz {
            while let Ok((job, result)) = p.rx.try_recv() {
                replies.extend(ptz_ctl.done(&job, result));
            }
            if let Some(job) = ptz_ctl.poll(now) {
                p.send(job);
            }
        }
        for r in replies {
            if let Some(v) = hub.get_mut(&r.to) {
                v.send_ptz(&ptz::reply(r.action, r.ok, r.unsupported, &r.message));
            }
        }

        // RTSP は見る人が 1 人でも居る間だけ 1 本
        match (&config, hub.is_empty(), camera.is_some()) {
            (_, true, true) => {
                camera = None; // TEARDOWN
                out(Out::Camera { ok: false, message: "停止".into() });
            }
            (Some((rtsp_url, _)), false, false) => camera = Some(Camera::open(rtsp_url)),
            _ => {}
        }
    }
}
