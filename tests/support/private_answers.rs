use std::path::Path;

pub fn make_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(windows)]
    {
        let who = std::process::Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .unwrap();
        assert!(who.status.success());
        let identity = String::from_utf8(who.stdout).unwrap();
        let sid = identity
            .lines()
            .next()
            .and_then(|line| line.rsplit(',').next())
            .map(|value| value.trim().trim_matches('"'))
            .filter(|value| value.starts_with("S-1-"))
            .expect("whoami /user must return the current user's SID");
        let principal = format!("*{sid}");
        let reset = std::process::Command::new("icacls")
            .arg(path)
            .arg("/reset")
            .output()
            .unwrap();
        assert!(
            reset.status.success(),
            "{}{}",
            String::from_utf8_lossy(&reset.stdout),
            String::from_utf8_lossy(&reset.stderr)
        );
        let owner = std::process::Command::new("icacls")
            .arg(path)
            .arg("/setowner")
            .arg(&principal)
            .output()
            .unwrap();
        assert!(
            owner.status.success(),
            "{}{}",
            String::from_utf8_lossy(&owner.stdout),
            String::from_utf8_lossy(&owner.stderr)
        );
        let output = std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{principal}:(F)"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(not(any(unix, windows)))]
    let _ = path;
}
