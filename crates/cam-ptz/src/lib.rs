//! カメラ (I-O DATA TS-NS320W 系) の首振り。I/O を持たない (TCP は host が持つ)。
//!
//! - 操作は公開仕様書 (iodata-solution/qwatch-api の `QwatchAPI_Specification.html`、対象 TS-NS310W) の CGI:
//!   - 方向: `POST /camera-cgi/com/ptz.cgi?move=up|down|left|right|home&stepSize=…` (1 回で 1 ステップ動く。
//!     開始/停止の形ではないので、押している間は [`Controller`] がステップを繰り返す)
//!   - プリセット: `POST /camera-cgi/com/ptzconfig.cgi?movepresetpoint=1..4` (位置の登録はカメラの管理画面で)
//!   - ズーム: 仕様に無い (光学ズームを持たない) → 非対応として返す
//! - 認証は HTTP Digest (RTSP と同じアカウント)。仕様の注記どおり API は同時 3 セッションまでなので、
//!   host からは常に 1 本ずつ (前の要求の応答を待ってから次) 送る。
//! - 見る人は最大 4 人。命令は最後に来たもの優先、移動は [`HOLD`] のあいだ押し直し (`move` の再送) が無ければ止まる。

use std::time::{Duration, Instant};

use cam_rtsp::digest::{self, Challenge};

/// `move` の再送がこれだけ無ければ離した (= stop) とみなす。見る側は押している間 300ms ごとに再送する
pub const HOLD: Duration = Duration::from_millis(1000);
/// ステップの要求どうしの最短の間 (前の応答が早く返っても、これより詰めて送らない)
pub const STEP_GAP: Duration = Duration::from_millis(200);
/// 1 回のステップの大きさ (`stepSize`、仕様の範囲は 0-2)
pub const STEP_SIZE: u8 = 1;
/// プリセットの番号の範囲 (仕様)
pub const PRESETS: std::ops::RangeInclusive<u8> = 1..=4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    pub const ALL: [Dir; 4] = [Dir::Up, Dir::Down, Dir::Left, Dir::Right];

    pub fn as_str(self) -> &'static str {
        match self {
            Dir::Up => "up",
            Dir::Down => "down",
            Dir::Left => "left",
            Dir::Right => "right",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.as_str() == s)
    }
}

/// 見る人からの命令 (データチャネル `ptz` の `action`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Move(Dir),
    Stop,
    Home,
    Preset(u8),
    Zoom,
}

impl Command {
    /// `{"action": …, "dir": …, "preset": …}` の中身から組む
    pub fn parse(action: &str, dir: Option<&str>, preset: Option<u64>) -> Result<Self, String> {
        match action {
            "move" => dir.and_then(Dir::parse).map(Command::Move).ok_or_else(|| "dir は up / down / left / right".into()),
            "stop" => Ok(Command::Stop),
            "home" => Ok(Command::Home),
            "preset" => match preset.and_then(|n| u8::try_from(n).ok()).filter(|n| PRESETS.contains(n)) {
                Some(n) => Ok(Command::Preset(n)),
                None => Err(format!("preset は {}〜{}", PRESETS.start(), PRESETS.end())),
            },
            "zoom" => Ok(Command::Zoom),
            _ => Err(format!("知らない action: {action}")),
        }
    }
}

/// カメラへ送る 1 回ぶん
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Step(Dir),
    Home,
    Preset(u8),
}

impl Op {
    /// CGI のパス (クエリ込み。Digest の uri にもこのまま使う)
    pub fn path(self) -> String {
        match self {
            Op::Step(d) => format!("/camera-cgi/com/ptz.cgi?move={}&stepSize={STEP_SIZE}", d.as_str()),
            Op::Home => "/camera-cgi/com/ptz.cgi?move=home".into(),
            Op::Preset(n) => format!("/camera-cgi/com/ptzconfig.cgi?movepresetpoint={n}"),
        }
    }

    /// 結果を返すときの `action`
    pub fn action(self) -> &'static str {
        match self {
            Op::Step(_) => "move",
            Op::Home => "home",
            Op::Preset(_) => "preset",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub op: Op,
    /// 命令した見る人 (結果はこの人へ返す)
    pub by: String,
    /// 成功も返す (押し続けの 2 回目以降のステップは失敗だけ返す)
    pub report: bool,
}

/// カメラが受けなかった理由。`unsupported` はカメラが CGI を持たない (404)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub unsupported: bool,
    pub message: String,
}

/// 見る人へ返す結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub to: String,
    pub action: &'static str,
    pub ok: bool,
    pub unsupported: bool,
    pub message: String,
}

impl Reply {
    fn ok(to: &str, action: &'static str) -> Self {
        Self { to: to.into(), action, ok: true, unsupported: false, message: String::new() }
    }

    fn fail(to: &str, action: &'static str, f: Failure) -> Self {
        Self { to: to.into(), action, ok: false, unsupported: f.unsupported, message: f.message }
    }
}

struct Held {
    dir: Dir,
    by: String,
    until: Instant,
    reported: bool,
}

/// カメラ 1 台ぶんの命令の交通整理。要求は常に 1 本だけ (応答を [`Controller::done`] で返すまで次を出さない)。
#[derive(Default)]
pub struct Controller {
    held: Option<Held>,
    once: Option<Job>,
    busy: bool,
    last_step: Option<Instant>,
}

impl Controller {
    /// 命令を受ける。すぐ返せる結果 (stop・非対応・不正) はここで返す
    pub fn command(&mut self, by: &str, cmd: Command, now: Instant) -> Option<Reply> {
        match cmd {
            Command::Move(dir) => {
                match &mut self.held {
                    // 押し続け (再送): 期限だけ延ばす
                    Some(h) if h.by == by && h.dir == dir => h.until = now + HOLD,
                    _ => self.held = Some(Held { dir, by: by.into(), until: now + HOLD, reported: false }),
                }
                self.once = None;
                None
            }
            Command::Stop => {
                // 他の人が後から動かしているなら、その人の移動は止めない
                if self.held.as_ref().is_some_and(|h| h.by == by) {
                    self.held = None;
                }
                Some(Reply::ok(by, "stop"))
            }
            Command::Home | Command::Preset(_) => {
                let op = if let Command::Preset(n) = cmd { Op::Preset(n) } else { Op::Home };
                self.held = None;
                self.once = Some(Job { op, by: by.into(), report: true });
                None
            }
            Command::Zoom => Some(Reply::fail(by, "zoom", Failure { unsupported: true, message: "このカメラはズームできません".into() })),
        }
    }

    /// 今カメラへ送るものがあれば返す (送ったら応答を `done` で返すこと)
    pub fn poll(&mut self, now: Instant) -> Option<Job> {
        if self.busy {
            return None;
        }
        if self.held.as_ref().is_some_and(|h| now >= h.until) {
            self.held = None; // 離したのに stop が届かなかった (自動 stop)
        }
        if let Some(job) = self.once.take() {
            self.busy = true;
            return Some(job);
        }
        let h = self.held.as_mut()?;
        if self.last_step.is_some_and(|t| now.saturating_duration_since(t) < STEP_GAP) {
            return None;
        }
        let report = !h.reported;
        h.reported = true;
        self.busy = true;
        self.last_step = Some(now);
        Some(Job { op: Op::Step(h.dir), by: h.by.clone(), report })
    }

    /// `poll` で出した要求の結果。見る人へ返すものがあれば返す
    pub fn done(&mut self, job: &Job, result: Result<(), Failure>) -> Option<Reply> {
        self.busy = false;
        match result {
            Ok(()) => job.report.then(|| Reply::ok(&job.by, job.op.action())),
            Err(f) => {
                // 失敗したら押し続けでも繰り返さない
                if matches!(job.op, Op::Step(_)) && self.held.as_ref().is_some_and(|h| h.by == job.by) {
                    self.held = None;
                }
                Some(Reply::fail(&job.by, job.op.action(), f))
            }
        }
    }

    /// 見る人が離れた。その人の押し続けは止める
    pub fn forget(&mut self, by: &str) {
        if self.held.as_ref().is_some_and(|h| h.by == by) {
            self.held = None;
        }
        if self.once.as_ref().is_some_and(|j| j.by == by) {
            self.once = None;
        }
    }
}

/// カメラの管理画面 (HTTP) の宛先と資格情報。RTSP URL のホスト・アカウントをそのまま使う
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    user: String,
    pass: String,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Target { .. }") // 資格情報・宛先はログに出さない
    }
}

impl Target {
    pub fn from_rtsp(rtsp_url: &str, http_port: u16) -> Result<Self, String> {
        let (host, _) = cam_rtsp::target(rtsp_url).map_err(|e| e.to_string())?;
        let u = cam_rtsp::parse_url(rtsp_url).map_err(|e| e.to_string())?;
        Ok(Self { host, port: http_port, user: u.user, pass: u.pass })
    }

    fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Digest の状態 (直前のチャレンジを使い回し、401 のときだけ取り直す)
pub struct Auth {
    target: Target,
    challenge: Option<Challenge>,
    nc: u32,
    cnonce: String,
}

impl Auth {
    pub fn new(target: Target, cnonce: &str) -> Self {
        Self { target, challenge: None, nc: 0, cnonce: cnonce.into() }
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// 送るバイト列 (1 要求 1 接続、`Connection: close`)
    pub fn request(&mut self, path: &str) -> Vec<u8> {
        self.build("POST", path, "Content-Length: 0\r\nConnection: close\r\n")
    }

    /// 接続を保ったまま本文を送り続ける要求 (通話の `audio/transmit.cgi`)。本文の長さは書かない
    pub fn stream_request(&mut self, path: &str) -> Vec<u8> {
        self.build("GET", path, "Content-Type: application/octet-stream\r\nConnection: Keep-Alive\r\nCache-Control: no-cache\r\n")
    }

    fn build(&mut self, method: &str, path: &str, headers: &str) -> Vec<u8> {
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\n{headers}", self.target.host_header());
        if let Some(ch) = &self.challenge {
            self.nc += 1;
            let t = &self.target;
            req += &format!("Authorization: {}\r\n", digest::authorization(ch, method, path, &t.user, &t.pass, self.nc, &self.cnonce));
        }
        req += "\r\n";
        req.into_bytes()
    }

    /// 401 を受けた。チャレンジを取り直し、送り直す価値があれば true
    /// (まだ認証していなかった・nonce が古くなった。同じチャレンジで落ちたなら資格情報が違う)
    pub fn on_unauthorized(&mut self, resp: &Response) -> bool {
        let next = resp
            .www_authenticate
            .iter()
            .filter_map(|v| digest::parse(v))
            .max_by_key(|c| matches!(c, Challenge::Digest { .. }));
        let retry = match (&self.challenge, &next) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(_), Some(Challenge::Digest { stale, .. })) => *stale,
            (Some(_), Some(Challenge::Basic)) => false,
        };
        self.challenge = next;
        self.nc = 0;
        retry
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub www_authenticate: Vec<String>,
    pub body: String,
}

/// 接続が閉じるまで読んだ応答を読む (chunked も解く)
pub fn parse_response(raw: &[u8]) -> Option<Response> {
    let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let mut www_authenticate = Vec::new();
    let mut chunked = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("www-authenticate") {
            www_authenticate.push(v.to_string());
        } else if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
    }
    let mut body = &raw[head_end + 4..];
    let mut out = Vec::new();
    if chunked {
        while let Some(i) = body.windows(2).position(|w| w == b"\r\n") {
            let size = std::str::from_utf8(&body[..i]).ok().and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok())?;
            body = &body[i + 2..];
            if size == 0 || body.len() < size {
                break;
            }
            out.extend_from_slice(&body[..size]);
            body = body.get(size + 2..).unwrap_or_default();
        }
    } else {
        out.extend_from_slice(body);
    }
    Some(Response { status, www_authenticate, body: String::from_utf8_lossy(&out).into_owned() })
}

/// 応答を結果にする。文言に資格情報・宛先は入れない
pub fn judge(resp: &Response) -> Result<(), Failure> {
    let fail = |message: &str| Err(Failure { unsupported: false, message: message.into() });
    match resp.status {
        200 if resp.body.trim_start().starts_with("ERROR") => fail("カメラが受け付けませんでした"),
        200 => Ok(()),
        401 => fail("カメラの認証に失敗しました"),
        404 => Err(Failure { unsupported: true, message: "このカメラは首振りに対応していません".into() }),
        s => Err(Failure { unsupported: false, message: format!("カメラが HTTP {s} を返しました") }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn parses_commands() {
        assert_eq!(Command::parse("move", Some("left"), None), Ok(Command::Move(Dir::Left)));
        assert!(Command::parse("move", Some("diagonal"), None).is_err());
        assert!(Command::parse("move", None, None).is_err());
        assert_eq!(Command::parse("stop", None, None), Ok(Command::Stop));
        assert_eq!(Command::parse("home", None, None), Ok(Command::Home));
        assert_eq!(Command::parse("preset", None, Some(4)), Ok(Command::Preset(4)));
        assert!(Command::parse("preset", None, Some(5)).is_err());
        assert!(Command::parse("preset", None, Some(0)).is_err());
        assert!(Command::parse("preset", None, Some(1 << 40)).is_err());
        assert!(Command::parse("preset", None, None).is_err());
        assert_eq!(Command::parse("zoom", Some("in"), None), Ok(Command::Zoom));
        assert!(Command::parse("dance", None, None).is_err());
    }

    #[test]
    fn paths_follow_the_spec() {
        assert_eq!(Op::Step(Dir::Up).path(), "/camera-cgi/com/ptz.cgi?move=up&stepSize=1");
        assert_eq!(Op::Home.path(), "/camera-cgi/com/ptz.cgi?move=home");
        assert_eq!(Op::Preset(3).path(), "/camera-cgi/com/ptzconfig.cgi?movepresetpoint=3");
        assert_eq!(Op::Preset(3).action(), "preset");
        assert_eq!(Op::Home.action(), "home");
    }

    #[test]
    fn holding_repeats_steps_one_at_a_time() {
        let now = t0();
        let mut c = Controller::default();
        assert_eq!(c.command("a", Command::Move(Dir::Left), now), None);
        let j = c.poll(now).unwrap();
        assert_eq!(j, Job { op: Op::Step(Dir::Left), by: "a".into(), report: true });
        // 応答待ちの間は出さない
        assert_eq!(c.poll(now + STEP_GAP * 2), None);
        assert_eq!(c.done(&j, Ok(())), Some(Reply::ok("a", "move")));
        // 応答が早くても STEP_GAP より詰めない
        assert_eq!(c.poll(now + STEP_GAP / 2), None);
        let j2 = c.poll(now + STEP_GAP).unwrap();
        assert!(!j2.report, "押し続けの 2 回目以降の成功は返さない");
        assert_eq!(c.done(&j2, Ok(())), None);
    }

    #[test]
    fn stops_by_itself_when_the_move_is_not_renewed() {
        let now = t0();
        let mut c = Controller::default();
        c.command("a", Command::Move(Dir::Up), now);
        let j = c.poll(now).unwrap();
        c.done(&j, Ok(()));
        // 再送で延びる
        c.command("a", Command::Move(Dir::Up), now + HOLD / 2);
        assert!(c.poll(now + HOLD).is_some_and(|j| !j.report));
        c.done(&Job { op: Op::Step(Dir::Up), by: "a".into(), report: false }, Ok(()));
        // 期限切れで自動 stop
        assert_eq!(c.poll(now + HOLD / 2 + HOLD), None);
        assert_eq!(c.poll(now + HOLD * 10), None);
    }

    #[test]
    fn stop_only_stops_your_own_move() {
        let now = t0();
        let mut c = Controller::default();
        c.command("a", Command::Move(Dir::Up), now);
        c.command("b", Command::Move(Dir::Down), now); // 最後の命令優先
        assert_eq!(c.command("a", Command::Stop, now), Some(Reply::ok("a", "stop")));
        let j = c.poll(now).unwrap();
        assert_eq!((j.op, j.by.as_str(), j.report), (Op::Step(Dir::Down), "b", true));
        c.done(&j, Ok(()));
        c.command("b", Command::Stop, now);
        assert_eq!(c.poll(now + STEP_GAP), None);
    }

    #[test]
    fn changing_direction_reports_again() {
        let now = t0();
        let mut c = Controller::default();
        c.command("a", Command::Move(Dir::Up), now);
        let j = c.poll(now).unwrap();
        c.done(&j, Ok(()));
        c.command("a", Command::Move(Dir::Right), now);
        let j = c.poll(now + STEP_GAP).unwrap();
        assert_eq!((j.op, j.report), (Op::Step(Dir::Right), true));
    }

    #[test]
    fn presets_win_over_moves_and_the_last_one_wins() {
        let now = t0();
        let mut c = Controller::default();
        c.command("a", Command::Move(Dir::Up), now);
        let j = c.poll(now).unwrap(); // 送信中
        c.command("b", Command::Preset(2), now);
        c.command("a", Command::Home, now); // 送信前に上書き
        assert_eq!(c.poll(now), None);
        c.done(&j, Ok(()));
        let j = c.poll(now).unwrap();
        assert_eq!(j, Job { op: Op::Home, by: "a".into(), report: true });
        assert_eq!(c.done(&j, Ok(())), Some(Reply::ok("a", "home")));
        // プリセットで移動は止まっている
        assert_eq!(c.poll(now + HOLD / 2), None);
        // 移動はまだ送っていないプリセットを取り消す
        c.command("b", Command::Preset(1), now);
        c.command("a", Command::Move(Dir::Left), now);
        assert_eq!(c.poll(now + STEP_GAP).map(|j| j.op), Some(Op::Step(Dir::Left)));
    }

    #[test]
    fn failures_are_always_reported_and_stop_the_hold() {
        let now = t0();
        let mut c = Controller::default();
        c.command("a", Command::Move(Dir::Up), now);
        let j = c.poll(now).unwrap();
        c.done(&j, Ok(()));
        let j = c.poll(now + STEP_GAP).unwrap();
        let f = Failure { unsupported: true, message: "x".into() };
        let r = c.done(&j, Err(f)).unwrap();
        assert!(!r.ok && r.unsupported && r.action == "move" && r.to == "a");
        assert_eq!(c.poll(now + STEP_GAP * 2), None);
        let f = Failure { unsupported: false, message: "y".into() };
        c.command("a", Command::Preset(1), now);
        let j = c.poll(now).unwrap();
        assert_eq!(c.done(&j, Err(f)).map(|r| (r.ok, r.action)), Some((false, "preset")));
    }

    #[test]
    fn zoom_is_unsupported() {
        let r = Controller::default().command("a", Command::Zoom, t0()).unwrap();
        assert!(!r.ok && r.unsupported && r.action == "zoom");
    }

    #[test]
    fn leaving_forgets_your_commands() {
        let now = t0();
        let mut c = Controller::default();
        c.command("a", Command::Move(Dir::Up), now);
        c.forget("b");
        c.forget("a");
        assert_eq!(c.poll(now), None);
        c.command("a", Command::Home, now);
        c.forget("a");
        assert_eq!(c.poll(now), None);
    }

    #[test]
    fn target_from_rtsp_url() {
        let t = Target::from_rtsp("rtsp://u%40x:p%3Aw@10.0.0.9:554/stream2", 80).unwrap();
        assert_eq!((t.host.as_str(), t.port, t.user.as_str(), t.pass.as_str()), ("10.0.0.9", 80, "u@x", "p:w"));
        assert_eq!(t.host_header(), "10.0.0.9");
        assert_eq!(format!("{t:?}"), "Target { .. }");
        let t = Target::from_rtsp("rtsp://10.0.0.9/s", 8080).unwrap();
        assert_eq!(t.host_header(), "10.0.0.9:8080");
        assert!(Target::from_rtsp("http://x", 80).is_err());
    }

    #[test]
    fn digest_handshake() {
        let mut a = Auth::new(Target::from_rtsp("rtsp://admin:pw@cam/s", 80).unwrap(), "c0ffee");
        assert_eq!(a.target().host, "cam");
        let first = String::from_utf8(a.request("/camera-cgi/com/ptz.cgi?move=up&stepSize=1")).unwrap();
        assert_eq!(
            first,
            "POST /camera-cgi/com/ptz.cgi?move=up&stepSize=1 HTTP/1.1\r\nHost: cam\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let r401 = parse_response(
            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\nWWW-Authenticate: Digest realm=\"cam\", nonce=\"n1\", qop=\"auth\"\r\n\r\n",
        )
        .unwrap();
        assert!(a.on_unauthorized(&r401), "初回の 401 は送り直す");
        let second = String::from_utf8(a.request("/p")).unwrap();
        assert!(second.contains("Authorization: Digest username=\"admin\", realm=\"cam\", nonce=\"n1\", uri=\"/p\""));
        assert!(second.contains("nc=00000001"));
        assert!(String::from_utf8(a.request("/p")).unwrap().contains("nc=00000002"), "チャレンジを使い回す");
        // 同じチャレンジで落ちた = 資格情報が違う。送り直さない
        assert!(!a.on_unauthorized(&r401));
        let stale = parse_response(b"HTTP/1.1 401 x\r\nWWW-Authenticate: Digest realm=\"cam\", nonce=\"n2\", stale=true\r\n\r\n").unwrap();
        assert!(a.on_unauthorized(&stale), "nonce が古くなっただけなら送り直す");
        let basic = parse_response(b"HTTP/1.1 401 x\r\nWWW-Authenticate: Basic realm=\"x\"\r\n\r\n").unwrap();
        assert!(!a.on_unauthorized(&basic));
        assert!(String::from_utf8(a.request("/p")).unwrap().contains("Authorization: Basic "));
        let none = parse_response(b"HTTP/1.1 401 x\r\n\r\n").unwrap();
        assert!(!a.on_unauthorized(&none));
        assert!(!String::from_utf8(a.request("/p")).unwrap().contains("Authorization"));
    }

    #[test]
    fn stream_request_is_keep_alive_get() {
        let mut a = Auth::new(Target::from_rtsp("rtsp://admin:pw@cam/s", 8080).unwrap(), "c0ffee");
        let first = String::from_utf8(a.stream_request("/camera-cgi/audio/transmit.cgi")).unwrap();
        assert_eq!(
            first,
            "GET /camera-cgi/audio/transmit.cgi HTTP/1.1\r\nHost: cam:8080\r\nContent-Type: application/octet-stream\r\nConnection: Keep-Alive\r\nCache-Control: no-cache\r\n\r\n"
        );
        let r401 = parse_response(b"HTTP/1.1 401 x\r\nWWW-Authenticate: Digest realm=\"cam\", nonce=\"n1\", qop=\"auth\"\r\n\r\n").unwrap();
        assert!(a.on_unauthorized(&r401));
        let second = String::from_utf8(a.stream_request("/camera-cgi/audio/transmit.cgi")).unwrap();
        assert!(second.starts_with("GET ") && second.contains("Authorization: Digest username=\"admin\""), "{second}");
    }

    #[test]
    fn parses_and_judges_responses() {
        let ok = parse_response(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nOK\n").unwrap();
        assert_eq!(ok.body, "OK\n");
        assert_eq!(judge(&ok), Ok(()));
        let chunked = parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nERR\r\n2;x=y\r\nOR\r\n0\r\n\r\n").unwrap();
        assert_eq!(chunked.body, "ERROR");
        assert!(judge(&chunked).is_err_and(|f| !f.unsupported));
        let cut = parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nOK").unwrap();
        assert_eq!(cut.body, "");
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_none());
        let nf = parse_response(b"HTTP/1.0 404 Not Found\r\n\r\n").unwrap();
        assert!(judge(&nf).is_err_and(|f| f.unsupported));
        let unauth = parse_response(b"HTTP/1.1 401 x\r\nbroken-line\r\n\r\n").unwrap();
        assert!(judge(&unauth).is_err_and(|f| f.message.contains("認証")));
        let busy = parse_response(b"HTTP/1.1 503 x\r\n\r\n").unwrap();
        assert!(judge(&busy).is_err_and(|f| f.message.contains("503")));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_none());
        assert!(parse_response(b"garbage\r\n\r\n").is_none());
    }
}
