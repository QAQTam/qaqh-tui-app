//! TUI 专有配置。与 daemon 的 ConfigDto/ConfigPatch、数据目录完全隔离。
//! 只读写本端专属 config.toml，拒绝未知字段、符号链接和超大文件，避免误覆盖。
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub performance_mode: bool,
}

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    #[cfg(test)]
    pub(crate) fn for_test(root: &Path) -> Self {
        Self {
            path: root.join("config.toml"),
        }
    }

    pub fn discover() -> Result<Self, String> {
        #[cfg(windows)]
        let root = std::env::var_os("APPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
        let root = root
            .filter(|path| path.is_absolute())
            .ok_or("无法确定 TUI 本地配置目录")?;
        Ok(Self {
            path: root.join("qaqh-tui").join("config.toml"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<LocalConfig, String> {
        if let Some(parent) = self.path.parent() {
            match std::fs::symlink_metadata(parent) {
                Ok(meta) if !meta.is_dir() => {
                    return Err("TUI 专属配置目录不能是符号链接或文件".into());
                }
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.to_string());
                }
                _ => {}
            }
        }
        let meta = match std::fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LocalConfig::default());
            }
            Err(error) => return Err(error.to_string()),
        };
        if !meta.is_file() || meta.len() > 65536 {
            return Err("TUI 配置必须是普通文件且不超过 64 KiB（拒绝符号链接）".into());
        }
        let text = std::fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        toml::from_str(&text).map_err(|e| format!("TUI 配置格式错误，未覆盖：{e}"))
    }

    pub fn save(&self, config: LocalConfig) -> Result<(), String> {
        // 保存前重读验证，绝不把别人的/未来版本的配置静默覆盖为当前字段。
        self.load()?;
        let parent = self.path.parent().ok_or("TUI 配置目录无效")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temp = parent.join(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<(), String> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp).map_err(|e| e.to_string())?;
            let text = toml::to_string(&config).map_err(|e| e.to_string())?;
            file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            drop(file);
            std::fs::rename(&temp, &self.path).map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_config_roundtrip_and_rejects_foreign_fields() {
        let root = std::env::temp_dir().join(format!("qaqh-local-{}", uuid::Uuid::new_v4()));
        let store = Store {
            path: root.join("config.toml"),
        };
        assert_eq!(store.load().unwrap(), LocalConfig::default());
        let config = LocalConfig {
            performance_mode: true,
        };
        store.save(config).unwrap();
        assert_eq!(store.load().unwrap(), config);
        let foreign = "model = 'do not overwrite'\n";
        std::fs::write(store.path(), foreign).unwrap();
        assert!(store.save(config).is_err());
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), foreign);
        let malformed = "performance_mode = [broken\n";
        std::fs::write(store.path(), malformed).unwrap();
        assert!(store.save(config).is_err());
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), malformed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn local_config_never_follows_file_symlinks() {
        let root = std::env::temp_dir().join(format!("qaqh-local-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("daemon.toml");
        std::fs::write(&target, "performance_mode = false\n").unwrap();
        let store = Store {
            path: root.join("config.toml"),
        };
        std::os::unix::fs::symlink(&target, store.path()).unwrap();
        assert!(store.load().is_err());
        assert!(
            store
                .save(LocalConfig {
                    performance_mode: true
                })
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "performance_mode = false\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn local_config_rejects_symlinked_private_directory() {
        let root = std::env::temp_dir().join(format!("qaqh-local-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::os::unix::fs::symlink(root.join("other"), root.join("private")).unwrap();
        let store = Store::for_test(&root.join("private"));
        assert!(store.save(LocalConfig::default()).is_err());
        assert!(!root.join("other/config.toml").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
