//! 見る側 (管理者の画面) から拠点 PC の自己更新を頼む。合図は首振りと同じデータチャネル `ptz` の `type: "update"`。
//! 中身は拡張の「今すぐ確認」(update_check) と同じ `update::check` で、結果は拡張と見る人全員へ返す。

/// 見る側からの `{"type":"update","action":"check"}` なら true
pub fn is_check(text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return false };
    v.get("type").and_then(serde_json::Value::as_str) == Some("update") && v.get("action").and_then(serde_json::Value::as_str) == Some("check")
}

/// データチャネルが開いたら送る (見る側はこれを見て版と「更新を確認」を出す)。拡張の版は読めなければ null
pub fn caps(host: &str, extension: Option<&str>) -> String {
    serde_json::json!({ "type": "update", "event": "caps", "host": host, "extension": extension }).to_string()
}

/// 確認の経過・結果 (`state` は拡張へ返すものと同じ: checking / latest / installed / deferred / failed / unsupported)
pub fn state(state: &str, current: &str, latest: Option<&str>, message: &str) -> String {
    serde_json::json!({ "type": "update", "event": "state", "state": state, "current": current, "latest": latest, "message": message }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_check_only() {
        assert!(is_check(r#"{"type":"update","action":"check"}"#));
        assert!(!is_check(r#"{"type":"update","action":"install"}"#));
        assert!(!is_check(r#"{"type":"ptz","action":"check"}"#));
        assert!(!is_check("x"));
    }

    #[test]
    fn message_shapes() {
        let v: serde_json::Value = serde_json::from_str(&caps("0.4.5", Some("0.4.5"))).unwrap();
        assert_eq!(v, serde_json::json!({"type":"update","event":"caps","host":"0.4.5","extension":"0.4.5"}));
        let v: serde_json::Value = serde_json::from_str(&caps("0.4.5", None)).unwrap();
        assert_eq!(v["extension"], serde_json::Value::Null);
        let v: serde_json::Value = serde_json::from_str(&state("installed", "0.4.5", Some("0.4.6"), "x")).unwrap();
        assert_eq!(v, serde_json::json!({"type":"update","event":"state","state":"installed","current":"0.4.5","latest":"0.4.6","message":"x"}));
    }
}
