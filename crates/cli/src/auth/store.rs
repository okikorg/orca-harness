//! Provider-neutral secure JSON credential persistence.

use std::{
    fs,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn write_private_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("credential path must be absolute".into());
    }
    let parent = path.parent().ok_or("invalid credential path")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(".oauth-{}-{nonce}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(value).unwrap())?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn refuses_relative_credential_paths() {
        assert!(super::write_private_json(
            std::path::Path::new("workspace-token.json"),
            &serde_json::json!({})
        )
        .is_err());
    }
}
