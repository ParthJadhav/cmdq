use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) fn absolute_path(path: PathBuf) -> Option<PathBuf> {
    path.is_absolute().then_some(path)
}

pub(crate) fn xdg_data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME").and_then(absolute_dir_from_env)
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .and_then(absolute_dir_from_env)
        .or_else(|| {
            dirs::home_dir()
                .and_then(absolute_path)
                .map(|home| home.join(".config"))
        })
}

fn absolute_dir_from_env(dir: OsString) -> Option<PathBuf> {
    if dir.is_empty() {
        return None;
    }
    let path = PathBuf::from(dir);
    path.is_absolute().then_some(path)
}

pub(crate) fn data_dir() -> Option<PathBuf> {
    xdg_data_home()
        .or_else(|| dirs::data_dir().and_then(absolute_path))
        .or_else(|| dirs::home_dir().and_then(absolute_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_data_home_requires_absolute_path() {
        assert_eq!(absolute_dir_from_env(OsString::from("")), None);
        assert_eq!(absolute_dir_from_env(OsString::from("relative/path")), None);

        let absolute = if cfg!(windows) {
            OsString::from(r"C:\Users\me\AppData\Local")
        } else {
            OsString::from("/tmp/cmdq-xdg")
        };
        assert_eq!(
            absolute_dir_from_env(absolute.clone()),
            Some(PathBuf::from(absolute))
        );
    }

    #[test]
    fn absolute_path_rejects_relative_fallbacks() {
        assert_eq!(absolute_path(PathBuf::from("relative")), None);

        let absolute = if cfg!(windows) {
            PathBuf::from(r"C:\Users\me")
        } else {
            PathBuf::from("/Users/me")
        };
        assert_eq!(absolute_path(absolute.clone()), Some(absolute));
    }
}
