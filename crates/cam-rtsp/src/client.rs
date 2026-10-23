//! RTSP の手順 (DESCRIBE → SETUP → PLAY) と、PLAY 後の interleaved RTP の受け取り。

use crate::digest::{self, Challenge};
use crate::h264::{codec_string, Depacketizer};
use crate::response::{self, Item, Response};
use crate::sdp::{self, VideoTrack};
use crate::url::{self, RtspUrl};
use crate::clock::Clock;
use crate::{err, Error};

const USER_AGENT: &str = "alc-cam-worker";

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 映像の codec 文字列 (WebCodecs の `avc1.PPCCLL`)。最初と SPS が変わったときに出る
    Codec(String),
    /// アクセスユニット 1 つ。最初の 1 つは必ずキーフレーム
    Frame { key: bool, timestamp_us: f64, data: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Describe,
    Setup,
    Play,
    Playing,
}

#[derive(Debug, Clone)]
struct Request {
    method: &'static str,
    uri: String,
    headers: Vec<(&'static str, String)>,
}

pub struct Client {
    url: RtspUrl,
    step: Step,
    cseq: u32,
    in_flight: Option<(u32, Request)>,
    challenge: Option<Challenge>,
    nc: u32,
    cnonce: String,
    track: Option<VideoTrack>,
    rtp_channel: u8,
    session: Option<String>,
    timeout_secs: u32,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    depack: Depacketizer,
    clock: Clock,
    codec: Option<String>,
}

impl Client {
    /// `rtsp_url` は `rtsp://user:pass@host:port/path`。`cnonce` は Digest 用の乱数 (呼び手が用意する)。
    /// 作った時点で DESCRIBE が出力に積まれる。
    pub fn new(rtsp_url: &str, cnonce: &str) -> Result<Self, Error> {
        let url = url::parse(rtsp_url)?;
        let mut c = Self {
            url,
            step: Step::Describe,
            cseq: 0,
            in_flight: None,
            challenge: None,
            nc: 0,
            cnonce: cnonce.to_string(),
            track: None,
            rtp_channel: 0,
            session: None,
            timeout_secs: 60,
            inbuf: Vec::new(),
            out: Vec::new(),
            depack: Depacketizer::default(),
            clock: Clock::default(),
            codec: None,
        };
        let uri = c.url.uri.clone();
        c.send(Request { method: "DESCRIBE", uri, headers: vec![("Accept", "application/sdp".into())] });
        Ok(c)
    }

    /// socket へ書くバイトを取り出す (空なら何もしない)
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    pub fn is_playing(&self) -> bool {
        self.step == Step::Playing
    }

    /// カメラが Session ヘッダで示した無通信の上限 (秒)。この半分程度の間隔で [`Client::keepalive`] を呼ぶ
    pub fn session_timeout_secs(&self) -> u32 {
        self.timeout_secs
    }

    /// PLAY 中に、セッションが切られないよう OPTIONS を積む (応答は読み捨てる)
    pub fn keepalive(&mut self) {
        if self.step != Step::Playing {
            return;
        }
        let uri = self.url.uri.clone();
        let req = Request { method: "OPTIONS", uri, headers: vec![] };
        let bytes = self.encode(&req);
        self.out.extend_from_slice(&bytes);
    }

    /// 終わるときに積む (送れなくても構わない)
    pub fn teardown(&mut self) {
        if self.session.is_none() {
            return;
        }
        let uri = self.url.uri.clone();
        let req = Request { method: "TEARDOWN", uri, headers: vec![] };
        let bytes = self.encode(&req);
        self.out.extend_from_slice(&bytes);
    }

    /// socket から読んだバイトを渡す
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Event>, Error> {
        self.inbuf.extend_from_slice(data);
        let mut events = Vec::new();
        while let Some(item) = response::next(&mut self.inbuf)? {
            match item {
                Item::Response(r) => self.on_response(r)?,
                Item::Other => {}
                Item::Interleaved { channel, data } => {
                    if self.step == Step::Playing && channel == self.rtp_channel {
                        self.on_rtp(&data, &mut events);
                    }
                }
            }
        }
        Ok(events)
    }

    fn on_rtp(&mut self, data: &[u8], events: &mut Vec<Event>) {
        let Some(track) = &self.track else { return };
        let (pt, rate) = (track.payload_type, track.clock_rate);
        for au in self.depack.push(data, pt) {
            if let Some(codec) = self.depack.sps().and_then(codec_string) {
                if self.codec.as_deref() != Some(codec.as_str()) {
                    self.codec = Some(codec.clone());
                    events.push(Event::Codec(codec));
                }
            }
            let timestamp_us = self.clock.micros(au.rtp_timestamp, rate);
            events.push(Event::Frame { key: au.key, timestamp_us, data: au.data });
        }
    }

    fn on_response(&mut self, r: Response) -> Result<(), Error> {
        let cseq = r.header("CSeq").and_then(|v| v.trim().parse::<u32>().ok());
        let Some((sent, req)) = self.in_flight.clone() else {
            // PLAY 後の keepalive の応答。セッションが消えていたら続けられない
            if r.status == 454 {
                return err("カメラがセッションを切りました");
            }
            return Ok(());
        };
        if cseq.is_some_and(|c| c != sent) {
            return Ok(());
        }
        self.in_flight = None;

        if r.status == 401 {
            let challenges: Vec<Challenge> = r.headers_named("WWW-Authenticate").filter_map(digest::parse).collect();
            let picked = challenges
                .iter()
                .find(|c| matches!(c, Challenge::Digest { .. }))
                .or_else(|| challenges.first())
                .cloned();
            let Some(picked) = picked else {
                return err("カメラが知らない方式の認証を求めています");
            };
            let stale = matches!(&picked, Challenge::Digest { stale: true, .. });
            if self.challenge.is_some() && !stale {
                return err("カメラの認証に失敗しました (ユーザー名・パスワード)");
            }
            if self.url.user.is_empty() {
                return err("カメラが認証を求めていますが、RTSP URL に資格情報がありません");
            }
            self.challenge = Some(picked);
            self.nc = 0;
            self.send(req);
            return Ok(());
        }
        if !(200..300).contains(&r.status) {
            return err(format!("{} が {} で失敗しました", req.method, r.status));
        }

        match self.step {
            Step::Describe => {
                let base = r
                    .header("Content-Base")
                    .or_else(|| r.header("Content-Location"))
                    .map(str::to_string)
                    .unwrap_or_else(|| self.url.uri.clone());
                let track = sdp::parse(&String::from_utf8_lossy(&r.body))?;
                self.depack = Depacketizer::new(track.sps.clone(), track.pps.clone());
                let uri = url::resolve_control(&base, &track.control);
                self.track = Some(track);
                self.step = Step::Setup;
                self.send(Request {
                    method: "SETUP",
                    uri,
                    headers: vec![("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1".into())],
                });
            }
            Step::Setup => {
                let Some(session) = r.header("Session") else {
                    return err("SETUP の応答に Session がありません");
                };
                let mut parts = session.split(';');
                self.session = parts.next().map(|s| s.trim().to_string());
                for p in parts {
                    if let Some(t) = p.trim().strip_prefix("timeout=").and_then(|t| t.parse().ok()) {
                        self.timeout_secs = t;
                    }
                }
                if let Some(ch) = r.header("Transport").and_then(interleaved_channel) {
                    self.rtp_channel = ch;
                }
                self.step = Step::Play;
                let uri = self.url.uri.clone();
                self.send(Request { method: "PLAY", uri, headers: vec![("Range", "npt=0.000-".into())] });
            }
            Step::Play => self.step = Step::Playing,
            Step::Playing => {}
        }
        Ok(())
    }

    fn send(&mut self, req: Request) {
        let bytes = self.encode(&req);
        self.out.extend_from_slice(&bytes);
        self.in_flight = Some((self.cseq, req));
    }

    fn encode(&mut self, req: &Request) -> Vec<u8> {
        self.cseq += 1;
        let mut s = format!("{} {} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: {USER_AGENT}\r\n", req.method, req.uri, self.cseq);
        if let Some(ch) = &self.challenge {
            self.nc += 1;
            let auth = digest::authorization(ch, req.method, &req.uri, &self.url.user, &self.url.pass, self.nc, &self.cnonce);
            s += &format!("Authorization: {auth}\r\n");
        }
        if let Some(session) = &self.session {
            s += &format!("Session: {session}\r\n");
        }
        for (k, v) in &req.headers {
            s += &format!("{k}: {v}\r\n");
        }
        s += "\r\n";
        s.into_bytes()
    }
}

fn interleaved_channel(transport: &str) -> Option<u8> {
    transport
        .split(';')
        .find_map(|p| p.trim().strip_prefix("interleaved="))
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::{build_rtp, packetize_nal};

    /// 送られたリクエストを 1 つずつ読む (偽カメラ側)
    fn requests(bytes: &[u8]) -> Vec<String> {
        String::from_utf8_lossy(bytes).split("\r\n\r\n").filter(|s| !s.is_empty()).map(str::to_string).collect()
    }

    fn reply(c: &mut Client, status: &str, cseq: u32, extra: &str, body: &str) -> Vec<Event> {
        let msg = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n{extra}Content-Length: {}\r\n\r\n{body}", body.len());
        c.feed(msg.as_bytes()).unwrap()
    }

    const SDP: &str = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z0LAHg==,aM4=\r\na=control:track1\r\n";

    #[test]
    fn full_handshake_with_digest_then_frames() {
        let mut c = Client::new("rtsp://u:p@cam:554/stream2", "cn").unwrap();
        let r = requests(&c.take_output());
        assert!(r[0].starts_with("DESCRIBE rtsp://cam:554/stream2 RTSP/1.0\r\nCSeq: 1"), "{}", r[0]);

        reply(&mut c, "401 Unauthorized", 1, "WWW-Authenticate: Basic realm=\"x\"\r\nWWW-Authenticate: Digest realm=\"x\", nonce=\"n\"\r\n", "");
        let r = requests(&c.take_output());
        assert!(r[0].starts_with("DESCRIBE") && r[0].contains("Authorization: Digest username=\"u\""), "{}", r[0]);

        reply(&mut c, "200 OK", 2, "Content-Base: rtsp://cam:554/stream2/\r\n", SDP);
        let r = requests(&c.take_output());
        assert!(r[0].starts_with("SETUP rtsp://cam:554/stream2/track1 RTSP/1.0"), "{}", r[0]);
        assert!(r[0].contains("interleaved=0-1"));

        reply(&mut c, "200 OK", 3, "Session: 12345;timeout=30\r\nTransport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n", "");
        let r = requests(&c.take_output());
        assert!(r[0].starts_with("PLAY") && r[0].contains("Session: 12345\r\n"), "{}", r[0]);
        assert_eq!(c.session_timeout_secs(), 30);

        reply(&mut c, "200 OK", 4, "", "");
        assert!(c.is_playing());

        // チャンネル 2 で IDR (FU-A 分割) と差分が来る
        let idr: Vec<u8> = std::iter::once(0x65).chain(vec![7u8; 2500]).collect();
        let mut wire = Vec::new();
        let parts = packetize_nal(&idr, 1200);
        for (i, p) in parts.iter().enumerate() {
            let rtp = build_rtp(100 + i as u16, 9000, i == parts.len() - 1, 96, p);
            wire.extend_from_slice(&[b'$', 2]);
            wire.extend_from_slice(&(rtp.len() as u16).to_be_bytes());
            wire.extend_from_slice(&rtp);
        }
        let rtp = build_rtp(100 + parts.len() as u16, 12000, true, 96, &[0x41, 1]);
        wire.extend_from_slice(&[b'$', 2]);
        wire.extend_from_slice(&(rtp.len() as u16).to_be_bytes());
        wire.extend_from_slice(&rtp);
        // RTCP (チャンネル 3) は無視される
        wire.extend_from_slice(&[b'$', 3, 0, 2, 0xaa, 0xbb]);

        // 半端に分けて渡しても組み上がる
        let (a, b) = wire.split_at(777);
        let mut ev = c.feed(a).unwrap();
        ev.extend(c.feed(b).unwrap());
        assert_eq!(ev[0], Event::Codec("avc1.42c01e".into()));
        let frames: Vec<(bool, f64)> = ev[1..]
            .iter()
            .map(|e| match e {
                Event::Frame { key, timestamp_us, .. } => (*key, *timestamp_us),
                _ => panic!(),
            })
            .collect();
        assert_eq!(frames, vec![(true, 0.0), (false, 3000.0 * 1e6 / 90000.0)]);

        c.keepalive();
        let r = requests(&c.take_output());
        assert!(r[0].starts_with("OPTIONS") && r[0].contains("Session: 12345"));
    }

    #[test]
    fn wrong_password_is_an_error() {
        let mut c = Client::new("rtsp://u:bad@cam/s", "cn").unwrap();
        c.take_output();
        reply(&mut c, "401 Unauthorized", 1, "WWW-Authenticate: Digest realm=\"x\", nonce=\"n\"\r\n", "");
        let msg = "RTSP/1.0 401 Unauthorized\r\nCSeq: 2\r\nWWW-Authenticate: Digest realm=\"x\", nonce=\"n\"\r\n\r\n";
        let e = c.feed(msg.as_bytes()).unwrap_err();
        assert!(e.0.contains("認証に失敗"), "{e}");
    }

    #[test]
    fn non_success_status_is_an_error() {
        let mut c = Client::new("rtsp://cam/s", "cn").unwrap();
        c.take_output();
        let msg = "RTSP/1.0 404 Not Found\r\nCSeq: 1\r\n\r\n";
        assert!(c.feed(msg.as_bytes()).is_err());
    }
}
