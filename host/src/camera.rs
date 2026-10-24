//! カメラの RTSP を受け続けるスレッド。見る人が 1 人でも居る間だけ、カメラにつき 1 本動く
//! (止める旗が立つまで、切れたら繋ぎ直す)。複数の見る人への複製は `hub` の仕事。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cam_rtsp::{AudioTrack, Client, Event};

pub enum Cam {
    Frame { key: bool, timestamp_us: f64, data: Vec<u8> },
    /// 音声のトラックがある (繋ぐたびに 1 度)
    AudioTrack(AudioTrack),
    /// 音声のまとまり 1 つ (AAC の AU / G.711)
    Audio(Vec<u8>),
    Status { ok: bool, message: String },
}

/// 受信スレッド 1 本の持ち手。落とすと旗が立ち、スレッドはカメラへ TEARDOWN を送って終わる
pub struct Camera {
    stop: Arc<AtomicBool>,
    pub rx: Receiver<Cam>,
}

impl Camera {
    pub fn open(rtsp_url: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        spawn(rtsp_url.to_string(), stop.clone(), tx);
        Self { stop, rx }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

const RETRY_MAX: Duration = Duration::from_secs(30);
/// これだけ何も読めなければ切れたものとみなす
const READ_STALL: Duration = Duration::from_secs(10);

fn spawn(rtsp_url: String, stop: Arc<AtomicBool>, tx: Sender<Cam>) {
    thread::spawn(move || {
        let mut wait = Duration::from_secs(1);
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            let reason = match run(&rtsp_url, &stop, &tx) {
                Ok(()) => return,
                Err(e) => e,
            };
            // 1 分以上流れていたなら、一時的な切断として待ちを戻す
            if started.elapsed() > Duration::from_secs(60) {
                wait = Duration::from_secs(1);
            }
            let msg = format!("{reason} — {}s 後に再接続", wait.as_secs());
            if tx.send(Cam::Status { ok: false, message: msg }).is_err() {
                return;
            }
            let until = Instant::now() + wait;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(100));
            }
            wait = (wait * 2).min(RETRY_MAX);
        }
    });
}

/// 止める旗で終わったら Ok、受信が続けられなければ Err (文言に資格情報・宛先は入れない)
fn run(rtsp_url: &str, stop: &AtomicBool, tx: &Sender<Cam>) -> Result<(), String> {
    let (host, port) = cam_rtsp::target(rtsp_url).map_err(|e| e.to_string())?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or("カメラの名前を引けません")?;
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| format!("カメラに繋がりません ({})", e.kind()))?;
    sock.set_read_timeout(Some(Duration::from_secs(1))).map_err(|e| e.to_string())?;
    let _ = sock.set_nodelay(true);

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut client = Client::new(rtsp_url, &format!("{nonce:x}")).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut last_read = Instant::now();
    let mut last_keepalive = Instant::now();
    let mut playing_reported = false;

    loop {
        let out = client.take_output();
        if !out.is_empty() {
            sock.write_all(&out).map_err(|e| format!("カメラへの送信に失敗 ({})", e.kind()))?;
        }
        if stop.load(Ordering::Relaxed) {
            teardown(&mut client, &mut sock);
            return Ok(());
        }

        let n = match sock.read(&mut buf) {
            Ok(0) => return Err("カメラが接続を閉じました".into()),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                if last_read.elapsed() > READ_STALL {
                    return Err("カメラから映像が来ません".into());
                }
                continue;
            }
            Err(e) => return Err(format!("カメラからの受信に失敗 ({})", e.kind())),
        };
        last_read = Instant::now();

        for ev in client.feed(&buf[..n]).map_err(|e| e.to_string())? {
            let msg = match ev {
                Event::Codec(codec) => {
                    playing_reported = true;
                    Cam::Status { ok: true, message: format!("受信中 ({codec})") }
                }
                Event::Frame { key, timestamp_us, data } => Cam::Frame { key, timestamp_us, data },
                Event::AudioTrack(t) => Cam::AudioTrack(t),
                Event::Audio { data } => Cam::Audio(data),
            };
            if tx.send(msg).is_err() {
                // 受け手が居ない = 畳まれた
                teardown(&mut client, &mut sock);
                return Ok(());
            }
        }
        if client.is_playing() && !playing_reported {
            playing_reported = true;
            let _ = tx.send(Cam::Status { ok: true, message: "受信開始 (キーフレーム待ち)".into() });
        }
        if client.is_playing() && last_keepalive.elapsed().as_secs() >= u64::from(client.session_timeout_secs() / 2).max(5) {
            client.keepalive();
            last_keepalive = Instant::now();
        }
    }
}

/// カメラ側のセッションを閉じる (送れなくても構わない。閉じないとカメラの同時接続枠を食い続ける機種がある)
fn teardown(client: &mut Client, sock: &mut TcpStream) {
    client.teardown();
    let _ = sock.write_all(&client.take_output());
    let _ = sock.flush();
}
