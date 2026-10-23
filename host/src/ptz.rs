//! 首振りの要求をカメラの管理画面 (HTTP, Digest) へ送るスレッド。要求は 1 本ずつ (交通整理は `cam_ptz::Controller`)。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cam_ptz::{Auth, Failure, Job, Target};

/// 管理画面の port の既定。e2e の偽カメラだけ `ALC_CAM_HTTP_PORT` で変える
const HTTP_PORT: u16 = 80;
const TIMEOUT: Duration = Duration::from_secs(3);
/// 応答の上限 (CGI の応答は数バイト)
const MAX_RESPONSE: usize = 64 * 1024;

/// 送信スレッド 1 本の持ち手。落とすとスレッドも終わる
pub struct Ptz {
    tx: Sender<Job>,
    pub rx: Receiver<(Job, Result<(), Failure>)>,
}

impl Ptz {
    pub fn open(rtsp_url: &str) -> Result<Self, String> {
        let port = std::env::var("ALC_CAM_HTTP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(HTTP_PORT);
        let target = Target::from_rtsp(rtsp_url, port)?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let mut auth = Auth::new(target, &format!("{nonce:x}"));
        let (tx, jobs) = mpsc::channel::<Job>();
        let (done, rx) = mpsc::channel();
        thread::spawn(move || {
            for job in jobs {
                let result = send(&mut auth, &job.op.path());
                if done.send((job, result)).is_err() {
                    return;
                }
            }
        });
        Ok(Self { tx, rx })
    }

    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

/// 1 要求。最初の 401 だけはチャレンジを取り直して送り直す
fn send(auth: &mut Auth, path: &str) -> Result<(), Failure> {
    let fail = |message: String| Failure { unsupported: false, message };
    let mut retried = false;
    loop {
        let req = auth.request(path);
        let raw = exchange(auth.target(), &req).map_err(fail)?;
        let resp = cam_ptz::parse_response(&raw).ok_or_else(|| fail("カメラの応答が読めません".into()))?;
        if resp.status == 401 && !retried && auth.on_unauthorized(&resp) {
            retried = true;
            continue;
        }
        return cam_ptz::judge(&resp);
    }
}

/// 文言に資格情報・宛先は入れない
fn exchange(t: &Target, req: &[u8]) -> Result<Vec<u8>, String> {
    let addr = (t.host.as_str(), t.port).to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or("カメラの名前を引けません")?;
    let mut sock = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| format!("カメラの管理画面に繋がりません ({})", e.kind()))?;
    sock.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
    sock.set_write_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
    sock.write_all(req).map_err(|e| format!("カメラへの送信に失敗 ({})", e.kind()))?;
    let mut buf = Vec::new();
    sock.take(MAX_RESPONSE as u64).read_to_end(&mut buf).map_err(|e| format!("カメラからの受信に失敗 ({})", e.kind()))?;
    Ok(buf)
}

/// データチャネルが開いたら最初に送る「できること」(見る側はこれを見てボタンを出す)
pub fn caps() -> String {
    serde_json::json!({
        "type": "ptz",
        "event": "caps",
        "dirs": cam_ptz::Dir::ALL.map(cam_ptz::Dir::as_str),
        "home": true,
        "presets": cam_ptz::PRESETS.collect::<Vec<_>>(),
        "zoom": false,
    })
    .to_string()
}

/// 見る側からの 1 通 → (`action`, 命令)。`type` が `ptz` でないものは捨てる (None)。読めない命令は `Err` (文言はそのまま返す)
pub fn parse(text: &str) -> Option<(String, Result<cam_ptz::Command, String>)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("type")?.as_str()? != "ptz" {
        return None;
    }
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
    let action = s("action").unwrap_or_default();
    Some((action.into(), cam_ptz::Command::parse(action, s("dir"), v.get("preset").and_then(serde_json::Value::as_u64))))
}

/// 見る側へ返す結果
pub fn reply(action: &str, ok: bool, unsupported: bool, message: &str) -> String {
    serde_json::json!({ "type": "ptz", "event": "result", "action": action, "ok": ok, "unsupported": unsupported, "message": message }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cam_ptz::{Command, Dir};

    #[test]
    fn caps_lists_what_the_camera_can_do() {
        let v: serde_json::Value = serde_json::from_str(&caps()).unwrap();
        assert_eq!(v["dirs"], serde_json::json!(["up", "down", "left", "right"]));
        assert_eq!(v["presets"], serde_json::json!([1, 2, 3, 4]));
        assert_eq!(v["zoom"], false);
    }

    #[test]
    fn parses_viewer_messages() {
        assert_eq!(parse(r#"{"type":"ptz","action":"move","dir":"up"}"#), Some(("move".into(), Ok(Command::Move(Dir::Up)))));
        assert_eq!(parse(r#"{"type":"ptz","action":"preset","preset":2}"#), Some(("preset".into(), Ok(Command::Preset(2)))));
        assert!(matches!(parse(r#"{"type":"ptz","action":"preset","preset":9}"#), Some((a, Err(_))) if a == "preset"));
        assert!(matches!(parse(r#"{"type":"ptz"}"#), Some((a, Err(_))) if a.is_empty()));
        assert_eq!(parse(r#"{"type":"other"}"#), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn reply_shape() {
        let v: serde_json::Value = serde_json::from_str(&reply("move", false, true, "x")).unwrap();
        assert_eq!(v, serde_json::json!({"type":"ptz","event":"result","action":"move","ok":false,"unsupported":true,"message":"x"}));
    }
}
