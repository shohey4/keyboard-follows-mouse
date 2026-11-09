//! Pre-shared key file: 64 hex characters, owner-only on Unix. Keychain (Mac)
//! and DPAPI (Windows) storage are not implemented.

use std::path::Path;

pub fn generate(path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| std::io::Error::other(e.to_string()))?;
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600); }
    let mut f = opts.open(path)?;
    f.write_all(hex.as_bytes())?;
    f.write_all(b"\n")
}

pub fn load(path: &Path) -> Result<[u8; 32], String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).map_err(|e| format!("key file: {e}"))?.permissions().mode();
        if mode & 0o077 != 0 { return Err(format!("key file must be owner-only (chmod 600); mode is {:o}", mode & 0o777)); }
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("key file: {e}"))?;
    let hex = text.trim();
    if hex.len() != 64 { return Err("key file must hold 64 hex characters".into()); }
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| "key file is not hex".to_string())?;
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_load_and_permission_check() {
        let dir = std::env::temp_dir().join(format!("kfm-keyfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let p = dir.join("k");
        generate(&p).unwrap();
        assert!(generate(&p).is_err(), "never overwrites an existing key");
        let k = load(&p).unwrap();
        assert_ne!(k, [0; 32]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load(&p).unwrap_err().contains("owner-only"));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
