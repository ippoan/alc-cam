//! alc-cam-host: 拠点の Chrome 拡張 (extension/) が native messaging で起動する。
//! 拡張が開いている間だけ動き、stdin が閉じたら (= 拡張が閉じたら) 終わる。
//!
//!   拡張 ──stdin──▶ config / start / stop / sdp_answer / ice_candidate   (config 以外は viewer_id 付き)
//!                   update_check   (自己更新。見る人が居ない時は拡張がこれだけのために起動する)
//!   拡張 ◀─stdout── ready / sdp_offer / camera / peer / log / update
//!
//! 首振りは映像と同じ PeerConnection のデータチャネル `ptz` で見る側から直接届き、カメラの管理画面 (HTTP) へ送る
//! (`ptz`。拡張は関わらない)。
//!
//! 音声も同じ PeerConnection (PCMU): カメラのマイク (RTSP の音声) は `cam_audio::Downlink` で PCMU にして全員へ。
//! 見る側の「押して話す」は同じデータチャネルの `talk` で届き (`cam_audio::Floor` が 1 人に絞る)、
//! その人の声を `talk::Talk` がカメラのスピーカー (`audio/transmit.cgi`) へ送る。
//!
//! 自己更新は拡張の update_check のほか、見る側からも同じデータチャネルの `update` で頼める (`remote`)。
//! 経過・結果は拡張と見る人全員へ返す。
//!
//! 見る人が来るたび (start) その人への WebRTC の offer を返す。カメラの RTSP は最初の 1 人で開き、
//! 最後の 1 人が離れたら TEARDOWN する (カメラにつき 1 本)。映像は `hub` が見る人ごとに複製して P2P で送る。

mod camera;
mod hub;
mod native;
mod peer;
mod ptz;
mod remote;
mod stun;
mod talk;
mod update;

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use camera::{Cam, Camera};
use hub::{Frame, Hub, MAX_VIEWERS};
use native::{In, Out};
use peer::Viewer;
use ptz::Ptz;
use talk::{Talk, TalkEvent};

const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    out(Out::Ready { version: VERSION });

    // 前回の自己更新で脇へ退けた古い exe を消す
    let exe = std::env::current_exe().ok();
    if let Some(exe) = &exe {
        update::cleanup(exe);
    }
    // 見る側へ出す版 (拡張はディスクの manifest.json。自己更新で変わったら読み直す)
    let mut update_caps = remote::caps(VERSION, exe.as_deref().and_then(update::extension_version).as_deref());
    let (upd_tx, upd_rx) = mpsc::channel::<Out>();
    let mut updating = false;
    // 拡張の入れ替えは見る人が居ない時だけ (再読み込みで配信が切れる)。更新のスレッドが見る
    let watching = Arc::new(AtomicUsize::new(0));

    let mut config: Option<(String, String)> = None;
    let mut hub: Hub<Viewer> = Hub::default();
    let mut camera: Option<Camera> = None;
    let mut ptz: Option<Ptz> = None;
    let mut ptz_ctl = cam_ptz::Controller::default();
    let mut downlink: Option<cam_audio::Downlink> = None;
    let mut floor = cam_audio::Floor::default();
    let mut talk: Option<Talk> = None;

    loop {
        // 話す人の入れ替わり (この巡で起きたもの)。最後に全員へ知らせ、カメラのスピーカーを開け閉めする
        let mut changes: Vec<cam_audio::Change> = Vec::new();
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
                Some((rtsp_url, stun)) => match Viewer::new(&viewer_id, rtsp_url, stun, update_caps.clone()) {
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
                changes.extend(floor.forget(&viewer_id));
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
            // 確認中に重ねて来たものは捨てる
            Some(Ok(In::UpdateCheck)) if updating => {}
            Some(Ok(In::UpdateCheck)) => {
                updating = true;
                start_update(upd_tx.clone(), watching.clone());
            }
        }

        while let Ok(m) = upd_rx.try_recv() {
            if let Out::Update { state, current, latest, message, .. } = &m {
                if *state != "checking" {
                    updating = false;
                    update_caps = remote::caps(VERSION, exe.as_deref().and_then(update::extension_version).as_deref());
                }
                let text = remote::state(state, current, latest.as_deref(), message);
                for viewer_id in hub.ids() {
                    if let Some(v) = hub.get_mut(&viewer_id) {
                        v.send_ptz(&text);
                    }
                }
            }
            out(m);
        }

        // カメラ → 全員へ複製
        if let Some(cam) = &camera {
            loop {
                match cam.rx.try_recv() {
                    Ok(Cam::Status { ok, message }) => out(Out::Camera { ok, message }),
                    Ok(Cam::Frame { key, timestamp_us, data }) => {
                        let f = Frame { key, ts: (timestamp_us * 0.09) as u64, data: data.into() };
                        for (viewer_id, _, message) in hub.on_frame(f) {
                            changes.extend(floor.forget(&viewer_id));
                            out(Out::Peer { viewer_id, ok: false, message });
                        }
                    }
                    Ok(Cam::AudioTrack(track)) => {
                        downlink = cam_audio::Downlink::new(&track)
                            .map_err(|e| out(Out::Log { message: format!("カメラの音は流せません: {e}") }))
                            .ok();
                        if downlink.is_some() {
                            out(Out::Log { message: format!("カメラの音: {}", track.describe()) });
                        }
                    }
                    Ok(Cam::Audio(unit)) => {
                        for pcmu in downlink.as_mut().map(|d| d.push(&unit)).unwrap_or_default() {
                            hub.on_audio(&pcmu);
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
                changes.extend(floor.forget(&viewer_id));
                out(Out::Peer { viewer_id, ok: false, message });
            }
            for (viewer_id, _, message) in hub.flush_connected() {
                changes.extend(floor.forget(&viewer_id));
                out(Out::Peer { viewer_id, ok: false, message });
            }
        }

        // 首振り: 見る側の命令 → 交通整理 → カメラへ 1 本ずつ → 結果を命令した人へ
        let now = Instant::now();
        let mut replies = Vec::new();
        let mut remote_check = false;
        for viewer_id in hub.ids() {
            let Some(v) = hub.get_mut(&viewer_id) else { continue };
            // 話している人の声だけをカメラへ。他の人の声は捨てる
            let mic = v.take_mic();
            if let (Some(t), Some(true)) = (&talk, floor.talker().map(|id| id == viewer_id)) {
                mic.into_iter().for_each(|p| t.send(p));
            }
            for text in v.take_ptz() {
                if remote::is_check(&text) {
                    remote_check = true;
                    continue;
                }
                match talk::parse(&text) {
                    Some(talk::Action::Start) => changes.extend(floor.start(&viewer_id, now)),
                    Some(talk::Action::Stop) => changes.extend(floor.stop(&viewer_id)),
                    None => {}
                }
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

        // 見る側からの更新の確認 (拡張の update_check と同じ。確認中なら重ねない)
        if remote_check && !updating {
            updating = true;
            out(Out::Log { message: "見る側から更新の確認を頼まれました".into() });
            start_update(upd_tx.clone(), watching.clone());
        }

        // 通話: カメラ側の結果を話そうとした人へ。繋がらなければその人を外す
        if let Some(t) = &talk {
            if let Ok(ev) = t.rx.try_recv() {
                let talker = floor.talker().map(str::to_string);
                match ev {
                    TalkEvent::Ready(format) => {
                        if let Some(v) = talker.as_deref().and_then(|id| hub.get_mut(id)) {
                            v.send_ptz(&talk::result(true, false, &format!("カメラのスピーカーに接続 ({format})")));
                        }
                    }
                    TalkEvent::Failed { busy, message } => {
                        out(Out::Log { message: format!("通話: {message}") });
                        if let Some(id) = talker {
                            if let Some(v) = hub.get_mut(&id) {
                                v.send_ptz(&talk::result(false, busy, &message));
                            }
                            changes.extend(floor.refuse(&id, Instant::now()));
                        }
                        talk = None;
                    }
                }
            }
        }
        changes.extend(floor.poll(now));
        if !changes.is_empty() {
            let talker = floor.talker().map(str::to_string);
            for viewer_id in hub.ids() {
                if let Some(v) = hub.get_mut(&viewer_id) {
                    v.send_ptz(&talk::state(talker.is_some(), talker.as_deref() == Some(viewer_id.as_str())));
                }
            }
            match (&config, &talker, talk.is_some()) {
                (Some((rtsp_url, _)), Some(_), false) => {
                    talk = Talk::open(rtsp_url).map_err(|e| out(Out::Log { message: format!("通話は使えません: {e}") })).ok();
                }
                (_, None, true) => talk = None, // 切断 = カメラ側の通話も終わる
                _ => {}
            }
        }

        // RTSP は見る人が 1 人でも居る間だけ 1 本
        watching.store(hub.len(), Ordering::Relaxed);

        match (&config, hub.is_empty(), camera.is_some()) {
            (_, true, true) => {
                camera = None; // TEARDOWN
                downlink = None;
                out(Out::Camera { ok: false, message: "停止".into() });
            }
            (Some((rtsp_url, _)), false, false) => camera = Some(Camera::open(rtsp_url)),
            _ => {}
        }
    }
}

fn update_msg(state: &'static str, latest: Option<String>, message: String) -> Out {
    Out::Update { state, current: VERSION, latest, message, extension_deferred: false }
}

/// 自己更新は別スレッドで (落とす間も映像を止めない)。結果は `tx` へ。入れ替えは Windows だけ
fn start_update(tx: mpsc::Sender<Out>, watching: Arc<AtomicUsize>) {
    if !cfg!(windows) {
        let _ = tx.send(update_msg("unsupported", None, "自己更新は Windows だけです".into()));
        return;
    }
    let _ = tx.send(update_msg("checking", None, String::new()));
    thread::spawn(move || {
        let busy = move || watching.load(Ordering::Relaxed) > 0;
        let res = std::env::current_exe()
            .map_err(|e| format!("自分の場所が分かりません: {e}"))
            .and_then(|exe| update::check(&update::GitHub::default(), VERSION, &exe, &busy));
        let _ = tx.send(match res {
            Ok(o) => {
                let (state, message) = update::summarize(&o);
                let extension_deferred = o.extension == update::Part::Deferred;
                Out::Update { state, current: VERSION, latest: Some(o.latest), message, extension_deferred }
            }
            Err(e) => update_msg("failed", None, e),
        });
    });
}
