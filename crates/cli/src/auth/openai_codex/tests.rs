use super::*;

#[test]
fn decodes_expiry_and_distinguishes_malformed_auth() {
    assert_eq!(jwt_exp("x.eyJleHAiOjEyMzQ1fQ.y"), Some(12345));
    let path = std::env::temp_dir().join(format!("orcacode-auth-bad-{}", std::process::id()));
    fs::write(&path, b"not-json").unwrap();
    assert_eq!(
        read_auth(&path).unwrap_err().kind,
        CredentialErrorKind::Malformed
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn refresh_contract_matches_official_codex_json_shape() {
    assert_eq!(
        refresh_form("refresh"),
        [
            ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
            ("grant_type", "refresh_token"),
            ("refresh_token", "refresh"),
        ]
    );
    assert!(needs_refresh(Some(1)));
    assert!(!needs_refresh(None));
}

#[test]
fn missing_auth_is_distinct_and_actionable() {
    let path = std::env::temp_dir().join(format!("orcacode-auth-missing-{}", std::process::id()));
    let error = read_auth(&path).unwrap_err();
    assert_eq!(error.kind, CredentialErrorKind::Missing);
}

#[tokio::test]
async fn unauthorized_recovery_reuses_a_concurrently_rotated_token() {
    let source = CodexCliCredential::new(PathBuf::from("/not/read"), "http://not-called".into());
    *source.state.lock().await = Some(CodexCredential {
        bearer: BearerCredential {
            access_token: "new".into(),
            expires_at: None,
        },
        account_id: "acct".into(),
    });
    assert_eq!(
        source
            .recover_unauthorized("old")
            .await
            .unwrap()
            .bearer
            .access_token,
        "new"
    );
}

#[test]
fn atomic_persistence_preserves_unknown_fields() {
    let dir = std::env::temp_dir().join(format!("orcacode-auth-dir-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("auth.json");
    let root = json!({"future": true, "tokens": {"access_token":"old", "refresh_token":"refresh", "account_id":"acct"}});
    fs::write(&path, serde_json::to_vec(&root).unwrap()).unwrap();
    let renewed = CodexCredential {
        bearer: BearerCredential {
            access_token: "new".into(),
            expires_at: None,
        },
        account_id: "acct".into(),
    };
    persist_tokens(&path, root, &renewed, "refresh").unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["future"], true);
    assert_eq!(saved["tokens"]["access_token"], "new");
    fs::remove_dir_all(dir).unwrap();
}
