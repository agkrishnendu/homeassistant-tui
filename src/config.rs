use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    url: Option<String>,
    token: Option<String>,
    token_command: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of the Home Assistant instance, e.g. `http://homeassistant.local:8123`.
    pub url: String,
    pub token: String,
}

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "homeassistant-tui")
            .map(|d| d.config_dir().join("config.toml"))
    }

    /// Resolve config from (highest precedence first): CLI/env overrides, then the config file.
    pub fn load(
        path: Option<&Path>,
        url_override: Option<String>,
        token_override: Option<String>,
    ) -> Result<Self> {
        let path = path.map(Path::to_path_buf).or_else(Self::default_path);
        let file = match &path {
            Some(p) if p.exists() => {
                let raw = std::fs::read_to_string(p)
                    .wrap_err_with(|| format!("reading {}", p.display()))?;
                let cfg: FileConfig =
                    toml::from_str(&raw).wrap_err_with(|| format!("parsing {}", p.display()))?;
                if cfg.token.is_some() {
                    warn_if_world_readable(p);
                }
                cfg
            }
            _ => FileConfig::default(),
        };

        let url = url_override.or(file.url).ok_or_else(|| {
            eyre!(
                "no Home Assistant URL configured; set HA_URL or `url` in {}",
                display_path(&path)
            )
        })?;

        let token = match (token_override, file.token, file.token_command) {
            (Some(t), _, _) => t,
            (None, Some(t), _) => t,
            (None, None, Some(cmd)) => run_token_command(&cmd)?,
            (None, None, None) => bail!(
                "no access token configured; set HA_TOKEN, or `token` / `token_command` in {}",
                display_path(&path)
            ),
        };

        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
        })
    }

    /// WebSocket endpoint derived from the base URL.
    pub fn ws_url(&self) -> String {
        ws_url(&self.url)
    }
}

pub fn ws_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let rest = if let Some(r) = base.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = base.strip_prefix("http://") {
        format!("ws://{r}")
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        base.to_string()
    } else {
        format!("ws://{base}")
    };
    if rest.ends_with("/api/websocket") {
        rest
    } else {
        format!("{rest}/api/websocket")
    }
}

fn display_path(p: &Option<PathBuf>) -> String {
    p.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "the config file".into())
}

/// Run `cmd` through the platform shell: `sh -c` on Unix, `cmd /C` on Windows.
fn shell(cmd: &str) -> Command {
    let (sh, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut c = Command::new(sh);
    c.arg(flag).arg(cmd);
    c
}

fn run_token_command(cmd: &str) -> Result<String> {
    let out = shell(cmd).output().wrap_err("running token_command")?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || token.is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "token_command `{cmd}` returned no token ({}{}).\n\
             Is the token stored? e.g. secret-tool store --label=\"homeassistant-tui Home Assistant token\" service homeassistant-tui",
            out.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    Ok(token)
}

#[cfg(unix)]
fn warn_if_world_readable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(p)
        && meta.permissions().mode() & 0o077 != 0
    {
        eprintln!(
            "warning: {} contains a token but is readable by other users; run `chmod 600 {}`",
            p.display(),
            p.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_from_http() {
        assert_eq!(
            ws_url("http://homeassistant.local:8123/"),
            "ws://homeassistant.local:8123/api/websocket"
        );
        assert_eq!(
            ws_url("https://x.ui.nabu.casa"),
            "wss://x.ui.nabu.casa/api/websocket"
        );
        assert_eq!(ws_url("10.0.0.2:8123"), "ws://10.0.0.2:8123/api/websocket");
        assert_eq!(
            ws_url("ws://h:8123/api/websocket"),
            "ws://h:8123/api/websocket"
        );
    }

    #[test]
    fn load_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            "url = \"http://file:8123\"\ntoken_command = \"echo abc\"\n",
        )
        .unwrap();
        let c = Config::load(Some(&p), None, None).unwrap();
        assert_eq!(c.url, "http://file:8123");
        assert_eq!(c.token, "abc");
        let c = Config::load(Some(&p), Some("http://env".into()), Some("tok".into())).unwrap();
        assert_eq!(c.url, "http://env");
        assert_eq!(c.token, "tok");
    }
}
