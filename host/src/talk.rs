//! 通話: 見る人の声 (PCMU) をカメラのスピーカーへ送るスレッドと、データチャネル `ptz` に載せる `talk` のメッセージ。
//!
//! 誰かが話している間だけ `audio/transmit.cgi` への TCP を 1 本保つ (話す人が替わっても繋ぎ直さない)。
//! 形式の変換 (PCMU → AAC) と応答の判定は `cam_audio`。順番は `cam_audio::Floor` (main が持つ)。

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cam_audio::transmit::{self, Format, Reply};
use cam_audio::Uplink;
use cam_ptz::{Auth, Target};

const TIMEOUT: Duration = Duration::from_secs(3);
/// 200 の後に形式が来るのを待つ時間。来なければ `audio/aac` とみなす
const TYPE_WAIT: Duration = Duration::from_millis(1500);
/// 1 回に書く上限 (仕様: 一度に受信できる最大サイズは 330 バイト程度)
const MAX_WRITE: usize = 330;

pub enum TalkEvent {
    /// カメラのスピーカーに繋がった (形式の名前)
    Ready(&'static str),
    /// 繋がらなかった・切れた。`busy` は他の人がカメラで通話中
    Failed { busy: bool, message: String },
}

/// 送信スレッド 1 本の持ち手。落とすと接続を切る (= カメラ側の通話も終わる)
pub struct Talk {
    tx: Sender<Vec<u8>>,
    pub rx: Receiver<TalkEvent>,
}

impl Talk {
    pub fn open(rtsp_url: &str) -> Result<Self, String> {
        let port = std::env::var("ALC_CAM_HTTP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(80);
        let target = Target::from_rtsp(rtsp_url, port)?;
        let (tx, voice) = mpsc::channel::<Vec<u8>>();
        let (events, rx) = mpsc::channel();
        thread::spawn(move || {
            let ev = match run(target, &voice, &events) {
                Ok(()) => return,
                Err((busy, message)) => TalkEvent::Failed { busy, message },
            };
            let _ = events.send(ev);
        });
        Ok(Self { tx, rx })
    }

    /// 見る人の PCMU のペイロード 1 つ
    pub fn send(&self, pcmu: Vec<u8>) {
        let _ = self.tx.send(pcmu);
    }
}

type Fail = (bool, String);

fn fail(message: impl Into<String>) -> Fail {
    (false, message.into())
}

/// 声の送り手 (`Talk`) が落ちたら Ok。文言に資格情報・宛先は入れない
fn run(target: Target, voice: &Receiver<Vec<u8>>, events: &Sender<TalkEvent>) -> Result<(), Fail> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut auth = Auth::new(target, &format!("{nonce:x}"));
    let (mut sock, format) = handshake(&mut auth)?;
    let rate = format.sample_rate().ok_or_else(|| fail(format!("カメラが {} しか受けません (AAC のみ対応)", format.describe())))?;
    let mut up = Uplink::new(rate).map_err(fail)?;
    if events.send(TalkEvent::Ready(format.describe())).is_err() {
        return Ok(());
    }
    // 繋ぐ間に溜まった声は捨てる (一度に送るとカメラ側で音が飛ぶ)
    while voice.try_recv().is_ok() {}
    sock.set_read_timeout(Some(Duration::from_millis(1))).map_err(|e| fail(e.to_string()))?;
    let mut scratch = [0u8; 512];
    loop {
        let pcmu = match voice.recv_timeout(Duration::from_millis(200)) {
            Ok(p) => Some(p),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        for frame in pcmu.map(|p| up.push_pcmu(&p)).unwrap_or_default() {
            for chunk in frame.chunks(MAX_WRITE) {
                sock.write_all(chunk).map_err(|e| fail(format!("カメラのスピーカーへの送信に失敗 ({})", e.kind())))?;
            }
        }
        // カメラが閉じたかを見る (中身は読み捨てる)
        match sock.read(&mut scratch) {
            Ok(0) => return Err(fail("カメラが通話を切りました")),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(fail(format!("カメラのスピーカーとの接続が切れました ({})", e.kind()))),
        }
    }
}

/// 認証 (401 なら繋ぎ直して送り直す) と形式の受け取り
fn handshake(auth: &mut Auth) -> Result<(TcpStream, Format), Fail> {
    let mut retried = false;
    loop {
        let mut sock = connect(auth.target())?;
        sock.write_all(&auth.stream_request(transmit::PATH)).map_err(|e| fail(format!("カメラへの送信に失敗 ({})", e.kind())))?;
        match read_reply(&mut sock)? {
            Reply::Ready(f) => return Ok((sock, f)),
            Reply::OkWithoutType => return Ok((sock, Format::Aac)),
            Reply::Busy => return Err((true, "他の人がカメラで通話中です".into())),
            Reply::Unauthorized(resp) if !retried && auth.on_unauthorized(&resp) => retried = true,
            Reply::Unauthorized(_) => return Err(fail("カメラの認証に失敗しました")),
            Reply::Failed(m) => return Err(fail(m)),
            Reply::Pending => return Err(fail("カメラの応答が読めません")),
        }
    }
}

fn connect(t: &Target) -> Result<TcpStream, Fail> {
    let addr = (t.host.as_str(), t.port).to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or_else(|| fail("カメラの名前を引けません"))?;
    let sock = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| fail(format!("カメラの管理画面に繋がりません ({})", e.kind())))?;
    sock.set_write_timeout(Some(TIMEOUT)).map_err(|e| fail(e.to_string()))?;
    let _ = sock.set_nodelay(true);
    Ok(sock)
}

/// 応答が決まるまで読む。200 で形式がまだなら [`TYPE_WAIT`] まで待つ
fn read_reply(sock: &mut TcpStream) -> Result<Reply, Fail> {
    sock.set_read_timeout(Some(Duration::from_millis(100))).map_err(|e| fail(e.to_string()))?;
    let started = Instant::now();
    let mut ok_since: Option<Instant> = None;
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => {
                // 閉じられた: ここまでで判定する (401 は閉じてくることがある)
                return Ok(match transmit::judge(&raw) {
                    Reply::OkWithoutType => Reply::Failed("カメラが通話を受け付けませんでした".into()),
                    r => r,
                });
            }
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(fail(format!("カメラからの受信に失敗 ({})", e.kind()))),
        }
        match transmit::judge(&raw) {
            Reply::Pending if started.elapsed() > TIMEOUT => return Err(fail("カメラが応答しません")),
            Reply::Pending => {}
            Reply::OkWithoutType => {
                if ok_since.get_or_insert_with(Instant::now).elapsed() > TYPE_WAIT {
                    return Ok(Reply::OkWithoutType);
                }
            }
            r => return Ok(r),
        }
        if raw.len() > 16 * 1024 {
            return Err(fail("カメラの応答が長すぎます"));
        }
    }
}

/// 見る側からの `talk` (`start` は押している間の送り直しを兼ねる)
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
}

/// `type` が `talk` でないものは None
pub fn parse(text: &str) -> Option<Action> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("type")?.as_str()? != "talk" {
        return None;
    }
    match v.get("action")?.as_str()? {
        "start" => Some(Action::Start),
        "stop" => Some(Action::Stop),
        _ => None,
    }
}

/// データチャネルが開いたら送る (見る側はこれを見て「押して話す」を出す)
pub fn caps() -> String {
    serde_json::json!({ "type": "talk", "event": "caps", "speak": true }).to_string()
}

/// 話している人が変わったとき全員へ。`mine` は受け取った人自身が話しているか
pub fn state(talking: bool, mine: bool) -> String {
    serde_json::json!({ "type": "talk", "event": "state", "talking": talking, "mine": mine }).to_string()
}

/// 話そうとした人へ、カメラ側の結果 (繋がった・繋がらなかった)
pub fn result(ok: bool, busy: bool, message: &str) -> String {
    serde_json::json!({ "type": "talk", "event": "result", "ok": ok, "busy": busy, "message": message }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_viewer_messages() {
        assert_eq!(parse(r#"{"type":"talk","action":"start"}"#), Some(Action::Start));
        assert_eq!(parse(r#"{"type":"talk","action":"stop"}"#), Some(Action::Stop));
        assert_eq!(parse(r#"{"type":"talk","action":"sing"}"#), None);
        assert_eq!(parse(r#"{"type":"ptz","action":"stop"}"#), None);
        assert_eq!(parse("x"), None);
    }

    #[test]
    fn message_shapes() {
        let v: serde_json::Value = serde_json::from_str(&caps()).unwrap();
        assert_eq!(v, serde_json::json!({"type":"talk","event":"caps","speak":true}));
        let v: serde_json::Value = serde_json::from_str(&state(true, false)).unwrap();
        assert_eq!(v, serde_json::json!({"type":"talk","event":"state","talking":true,"mine":false}));
        let v: serde_json::Value = serde_json::from_str(&result(false, true, "x")).unwrap();
        assert_eq!(v, serde_json::json!({"type":"talk","event":"result","ok":false,"busy":true,"message":"x"}));
    }
}
