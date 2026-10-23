//! カメラの宛先 = VPC Service の binding 名 (`cameras.rec_ip` 列に入れる) の検査。
//! **登録の時と、カメラへ fetch する直前の両方で通す。**
//!
//! 宛先の IP・ポートは Cloudflare 側の VPC Service に固定され (ダッシュボードの Workers VPC で作る)、Worker は
//! wrangler.toml の `[[vpc_services]]` の binding を名前で引くだけ。DB に入るのは binding 名なので、ここでは名前の形
//! (`^CAM_[A-Z0-9_]{1,60}$`) だけを見る。その binding がこの Worker に在るかは Worker が実行時に確かめる
//! (無ければ登録時 400 / fetch 時 500)。

/// 検査に通った binding 名。作れるのは [`VpcBinding::parse`] だけ
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpcBinding(String);

/// `CAM_` の後に続けられる長さの上限
const MAX_SUFFIX: usize = 60;

/// 形は合うが VPC Service ではない binding (wrangler.toml の `[[hyperdrive]]`)。宛先にさせない
const RESERVED: &[&str] = &["CAM_HYPERDRIVE"];

impl VpcBinding {
    /// binding 名として受けるなら `Some`。文字列を直さない (小文字・前後の空白は拒否)
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("CAM_")?;
        let ok = (1..=MAX_SUFFIX).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        (ok && !RESERVED.contains(&s)).then(|| Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_binding_names() {
        let longest = format!("CAM_{}", "A".repeat(MAX_SUFFIX));
        for ok in [
            "CAM_TS_NS320W_55",
            "CAM_A",
            "CAM_1",
            "CAM__",
            longest.as_str(),
        ] {
            let b = VpcBinding::parse(ok).unwrap_or_else(|| panic!("{ok} は通るはず"));
            assert_eq!(b.as_str(), ok);
        }
    }

    #[test]
    fn rejects_everything_else() {
        let too_long = format!("CAM_{}", "A".repeat(MAX_SUFFIX + 1));
        for ng in [
            // 小文字
            "cam_ts_ns320w_55",
            "CAM_ts_ns320w_55",
            "Cam_A",
            // CAM_ が無い
            "TS_NS320W_55",
            "CAM",
            "CAM_",
            "CAMERA_A",
            "SSO_ENCRYPTION_KEY",
            // VPC Service ではない binding
            "CAM_HYPERDRIVE",
            "RECORDINGS",
            // 空白・空
            " CAM_A",
            "CAM_A ",
            "CAM_A\n",
            "CAM_ A",
            "",
            // 長すぎる
            too_long.as_str(),
            // IP・URL・記号
            "192.168.1.64",
            "CAM_192.168.1.64",
            "http://camera.vpc.internal",
            "CAM_A-B",
            "CAM_Ａ",
        ] {
            assert_eq!(VpcBinding::parse(ng), None, "{ng:?} は落ちるはず");
        }
    }
}
