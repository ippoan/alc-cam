//! 見る人 1 人ぶんの WebRTC (str0m)。UDP は 1 人に 1 本。カメラの受信は持たない (`hub` が束ねて配る)。
//! カメラ中継規約どおり、映像のみ・H.264 パススルー・non-trickle の offer (候補は offer に全部入れる)。

use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use crate::hub::{Frame, Sink};
use crate::native::Out;
use crate::stun;

/// answer が来てから繋がるまでの猶予
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Viewer {
    /// cam-room の DO が付けた viewer_id (host → 拡張のメッセージに付けて返す)
    id: String,
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    mid: Mid,
    pt: Option<Pt>,
    pending: Option<SdpPendingOffer>,
    answered_at: Option<Instant>,
    connected: bool,
    buf: Vec<u8>,
}

impl Viewer {
    /// offer (SDP 文字列) を作る。`rtsp_url` は LAN 側の IP を決めるためだけに使う
    pub fn new(id: &str, rtsp_url: &str, stun_server: &str) -> Result<(Self, String), String> {
        let stun_addr = resolve(stun_server.trim_start_matches("stun:"));
        let lan_ip = route_ip(rtsp_url, stun_addr).ok_or("LAN のアドレスが分かりません")?;
        let socket = UdpSocket::bind((lan_ip, 0)).map_err(|e| format!("UDP を開けません ({})", e.kind()))?;
        let local = socket.local_addr().map_err(|e| e.to_string())?;

        let mut rtc = RtcConfig::new().clear_codecs().enable_h264(true).build(Instant::now());
        let host = Candidate::host(local, "udp").map_err(|e| format!("host 候補: {e}"))?;
        rtc.add_local_candidate(host);
        if let Some(server) = stun_addr {
            if let Some(public) = stun::query(&socket, server, txid()) {
                if let Ok(c) = Candidate::server_reflexive(public, local, "udp") {
                    rtc.add_local_candidate(c);
                }
            }
        }

        let mut api = rtc.sdp_api();
        let mid = api.add_media(MediaKind::Video, Direction::SendOnly, Some("alc-cam".into()), Some("camera".into()), None);
        let (offer, pending) = api.apply().ok_or("offer を作れません")?;

        let viewer = Self {
            id: id.to_string(),
            rtc,
            socket,
            local,
            mid,
            pt: None,
            pending: Some(pending),
            answered_at: None,
            connected: false,
            buf: vec![0u8; 2000],
        };
        Ok((viewer, offer.to_sdp_string()))
    }

    pub fn accept_answer(&mut self, sdp: &str) -> Result<(), String> {
        let pending = self.pending.take().ok_or("offer を出していないのに answer が来ました")?;
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|e| format!("answer が読めません: {e}"))?;
        self.rtc.sdp_api().accept_answer(pending, answer).map_err(|e| format!("answer を受けられません: {e}"))?;
        self.answered_at = Some(Instant::now());
        Ok(())
    }

    /// 見る側からの trickle 候補 (任意)。読めないものは捨てる
    pub fn add_candidate(&mut self, v: &Value) {
        let Some(s) = v.get("candidate").and_then(Value::as_str).filter(|s| !s.is_empty()) else { return };
        let s = s.trim_start_matches("a=");
        if let Ok(c) = Candidate::from_sdp_string(s) {
            self.rtc.add_remote_candidate(c);
        }
    }

    /// 1 巡ぶん進める (UDP は最大 `max_wait` 待つ)。`Ok(false)` は相手が切れた (畳んでよい)
    pub fn step(&mut self, max_wait: Duration, out: &mut dyn FnMut(Out)) -> Result<bool, String> {

        let timeout = loop {
            match self.rtc.poll_output().map_err(|e| e.to_string())? {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Output::Event(Event::Connected) => {
                    self.connected = true;
                    out(Out::Peer { viewer_id: self.id.clone(), ok: true, message: "接続 (P2P)".into() });
                }
                Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => return Ok(false),
                Output::Event(_) => {}
            }
        };
        if !self.connected && self.answered_at.is_some_and(|t| t.elapsed() > CONNECT_TIMEOUT) {
            return Err("見る側と繋がりませんでした (NAT 越えに失敗した可能性)".into());
        }

        let now = Instant::now();
        let wait = timeout.saturating_duration_since(now).min(max_wait);
        if wait.is_zero() {
            self.rtc.handle_input(Input::Timeout(now)).map_err(|e| e.to_string())?;
            return Ok(true);
        }
        self.socket.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
        self.buf.resize(2000, 0);
        let input = match self.socket.recv_from(&mut self.buf) {
            Ok((n, source)) => {
                let Ok(contents) = self.buf[..n].try_into() else { return Ok(true) };
                Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: self.local, contents })
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Input::Timeout(Instant::now()),
            // Windows は相手の port が閉じていると ConnectionReset を返す。致命ではない
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Ok(true),
            Err(e) => return Err(format!("UDP の受信に失敗 ({})", e.kind())),
        };
        self.rtc.handle_input(input).map_err(|e| e.to_string())?;
        Ok(true)
    }

}

impl Sink for Viewer {
    fn is_connected(&self) -> bool {
        self.connected
    }

    fn write(&mut self, f: &Frame) -> Result<(), String> {
        let Some(writer) = self.rtc.writer(self.mid) else { return Ok(()) };
        let pt = match self.pt {
            Some(pt) => pt,
            None => {
                let Some(pt) = writer.payload_params().find(|p| p.spec().codec == Codec::H264).map(|p| p.pt()) else {
                    return Err("見る側が H.264 を受けません".into());
                };
                self.pt = Some(pt);
                pt
            }
        };
        writer.write(pt, Instant::now(), MediaTime::from_90khz(f.ts), f.data.clone()).map_err(|e| e.to_string())
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.rtc.disconnect();
    }
}

fn resolve(hostport: &str) -> Option<SocketAddr> {
    hostport.to_socket_addrs().ok()?.find(SocketAddr::is_ipv4)
}

/// カメラ (LAN) へ向かうときに使うこちらの IP。UDP を connect するだけで経路表を引ける (パケットは出ない)。
/// カメラの名前が引けなければ STUN サーバーへの経路で代える
fn route_ip(rtsp_url: &str, stun_addr: Option<SocketAddr>) -> Option<IpAddr> {
    let camera = cam_rtsp::target(rtsp_url).ok().and_then(|(h, p)| resolve(&format!("{h}:{p}")));
    for dest in [camera, stun_addr].into_iter().flatten() {
        let s = UdpSocket::bind("0.0.0.0:0").ok()?;
        if s.connect(dest).is_ok() {
            if let Ok(a) = s.local_addr() {
                if !a.ip().is_unspecified() {
                    return Some(a.ip());
                }
            }
        }
    }
    None
}

fn txid() -> [u8; 12] {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut id = [0u8; 12];
    id.copy_from_slice(&n.to_le_bytes()[..12]);
    id
}
