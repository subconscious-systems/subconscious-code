//! Native Windows profile paths; Unix HOME behavior is unchanged.
use std::ffi::OsString;
use std::path::PathBuf;

fn profile_dir(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(profile) = get("USERPROFILE")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Some(profile);
    }
    let mut profile = get("HOMEDRIVE")?;
    profile.push(get("HOMEPATH")?);
    let profile = PathBuf::from(profile);
    profile.is_absolute().then_some(profile)
}

/// `%USERPROFILE%\.sc`, with the standard HOMEDRIVE/HOMEPATH fallback.
pub fn user_dir() -> Option<PathBuf> {
    profile_dir(|key| std::env::var_os(key)).map(|p| p.join(".sc"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_profile_paths_do_not_require_home() {
        let get = |key: &str| match key {
            "USERPROFILE" => Some(OsString::from("C:\\Users\\space 日本語")),
            "HOME" => Some(OsString::from("/wrong-unix-home")),
            _ => None,
        };
        assert_eq!(
            profile_dir(get),
            Some(PathBuf::from("C:\\Users\\space 日本語"))
        );
        assert_eq!(
            profile_dir(|key| match key {
                "HOMEDRIVE" => Some("D:".into()),
                "HOMEPATH" => Some("\\Users\\test".into()),
                _ => None,
            }),
            Some(PathBuf::from("D:\\Users\\test"))
        );
        assert_eq!(profile_dir(|_| None), None);
        assert_eq!(profile_dir(|_| Some("relative".into())), None);
    }
}
