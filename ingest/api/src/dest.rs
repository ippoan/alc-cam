//! カメラの宛先 (`cameras.rec_ip`) の検査。**登録の時と、カメラへ fetch する直前の両方で通す。**
//!
//! VPC Network の binding (`CAM_NET`) は宛先を絞らない (公開インターネットにも、Tunnel の先の網の任意の
//! IP・ポートにも出られる。ippoan/alc-cam#2 の c0 で実測) ので、Worker 側で次だけを許す:
//! - IPv4 のリテラルだけ (名前・IPv6・ポート付き・前後の空白は拒否)
//! - 私設帯 (10/8・172.16/12・192.168/16) の中だけ (loopback・link-local・0/8・multicast・broadcast は私設帯の外)
//! - URL は `http://<IP>` (ポート 80 固定、https なし) — [`base_url`] だけが組み立てる

use std::net::Ipv4Addr;

/// 検査に通った宛先 (正準形の IPv4)。作れるのは [`Dest::parse`] だけ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dest(Ipv4Addr);

impl Dest {
    /// 宛先として受けるなら `Some`。文字列を直さない (前後の空白・ゼロ埋め・ポートは拒否)
    pub fn parse(s: &str) -> Option<Self> {
        // std の parser はゼロ埋め (`010.0.0.1`) も拒むので、正準形と一致しない文字列はここで落ちる
        let ip: Ipv4Addr = s.parse().ok()?;
        (ip.to_string() == s && ip.is_private()).then_some(Self(ip))
    }

    /// 保存する形 (`Ipv4Addr::to_string()`)
    pub fn canonical(&self) -> String {
        self.0.to_string()
    }

    /// カメラの URL の頭 (`http://<IP>`、ポート 80)
    pub fn base_url(&self) -> String {
        format!("http://{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_private_ipv4() {
        for ok in ["10.0.0.1", "172.16.5.20", "192.168.1.64", "172.31.255.254"] {
            let d = Dest::parse(ok).unwrap_or_else(|| panic!("{ok} は通るはず"));
            assert_eq!(d.canonical(), ok);
            assert_eq!(d.base_url(), format!("http://{ok}"));
        }
    }

    #[test]
    fn rejects_everything_else() {
        for ng in [
            // 公開 IP
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "192.169.0.1",
            // 名前
            "camera.local",
            "localhost",
            "example.com",
            // IPv6
            "::1",
            "fd00::1",
            "::ffff:192.168.1.1",
            // ポート付き
            "192.168.1.64:80",
            "192.168.1.64:8080",
            // loopback・link-local・0/8・multicast・broadcast
            "127.0.0.1",
            "169.254.169.254",
            "169.254.1.1",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            // 空白・空・正準形でない
            " 192.168.1.64",
            "192.168.1.64 ",
            "192.168.1.64\n",
            "",
            "192.168.001.064",
            "192.168.1",
            "http://192.168.1.64",
            "192.168.1.64/",
        ] {
            assert_eq!(Dest::parse(ng), None, "{ng:?} は落ちるはず");
        }
    }
}
