//! 設定ファイル（client だけが読む）。

/// `~/.berth/config.toml` の中身。読むのは **berth（client）だけ**。シェルを決めているのが client で、
/// berthd は渡された値で起こすだけ。置き場は journal・ペインファイルと同じ `~/.berth/`（OS で分けない）。
///
/// ```toml
/// default-shell = "hsh"
///
/// [env]
/// CLAUDE_CODE_SHELL = "~/.local/libexec/hsh-as-bash/bash"
/// ```
///
/// - `default-shell`: 新しく作るセッションのシェル。優先順位は `-s` > `BERTHD_SHELL` > **これ** > 親のシェル > `$SHELL`。
/// - `[env]`: **新しく作るときだけ**セッションに足す環境変数（`Spawn.env`）。既存のセッションに attach するときは足さない。
/// - 値の先頭の `~` / `~/` だけ home に展開する。`$VAR` は展開しない（規則を増やさない）。
/// - 知らないキー・型違い・壊れた TOML は黙って無視しない。読めなかったと 1 度名乗り、設定なしとして続ける
///   （attach は止めない）。
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BerthConfig {
    #[serde(default, rename = "default-shell")]
    pub default_shell: Option<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

impl BerthConfig {
    /// `Spawn.env` に載せる形（キー順・空なら空）。
    #[must_use]
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// 設定ファイルを読んだ結果。「無い」「読めた」「壊れている」は**別の事実**（混ぜると壊れた設定が「無い」に見える）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigOutcome {
    /// ファイルが無い（既定）。
    Missing { path: std::path::PathBuf },
    /// 読めた。
    Loaded {
        path: std::path::PathBuf,
        config: BerthConfig,
    },
    /// 在るが読めない（TOML の誤り・知らないキー・型違い・読み取りの失敗）。設定なしとして扱う。
    Broken {
        path: std::path::PathBuf,
        error: String,
    },
}

impl ConfigOutcome {
    /// 読めた設定（無い・壊れているなら `None`）。
    #[must_use]
    pub fn config(&self) -> Option<&BerthConfig> {
        match self {
            Self::Loaded { config, .. } => Some(config),
            _ => None,
        }
    }

    /// どのファイルの話か。
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::Missing { path } | Self::Loaded { path, .. } | Self::Broken { path, .. } => path,
        }
    }
}

/// 設定ファイルの場所（純粋）: `BERTH_CONFIG` が在ればそれ、無ければ `<home>/.berth/config.toml`。home も無ければ `None`。
#[must_use]
pub fn config_path_from(
    override_path: Option<&std::ffi::OsStr>,
    home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if let Some(p) = override_path.filter(|p| !p.is_empty()) {
        return Some(std::path::PathBuf::from(p));
    }
    Some(home?.join(".berth").join("config.toml"))
}

/// 設定ファイルの場所（環境変数と home から）。
#[must_use]
pub fn config_path() -> Option<std::path::PathBuf> {
    config_path_from(
        std::env::var_os("BERTH_CONFIG").as_deref(),
        dirs::home_dir().as_deref(),
    )
}

/// 本文を読む（純粋）。`home` は `~` の展開先（無ければ展開しない）。
pub fn parse_config(text: &str, home: Option<&std::path::Path>) -> Result<BerthConfig, String> {
    let mut cfg: BerthConfig =
        toml::from_str(text).map_err(|e| one_line_toml_error(&e.to_string()))?;
    if let Some(s) = cfg.default_shell.as_mut() {
        *s = expand_tilde(s, home);
    }
    for v in cfg.env.values_mut() {
        *v = expand_tilde(v, home);
    }
    Ok(cfg)
}

/// toml の誤りは複数行（場所の見出し・抜粋・理由）で来る。人に見せる 1 行は **理由 ＋ 場所**
/// （見出しだけ出すと「TOML parse error at line 1, column 1」で、知らないキーだったことが分からない）。
pub(crate) fn one_line_toml_error(raw: &str) -> String {
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    let head = lines.first().copied().unwrap_or("");
    let reason = lines.last().copied().unwrap_or("");
    let at = head.strip_prefix("TOML parse error at ").unwrap_or("");
    if reason.is_empty() || reason == head {
        head.to_string()
    } else if at.is_empty() {
        format!("{reason} ({head})")
    } else {
        format!("{reason} ({at})")
    }
}

/// 先頭の `~` / `~/…` だけ home に置き換える（それ以外はそのまま）。
pub(crate) fn expand_tilde(value: &str, home: Option<&std::path::Path>) -> String {
    let Some(home) = home else {
        return value.to_string();
    };
    if value == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        // 文字列で繋ぐ（`Path::join` は Windows で `\` を挟み、設定に書いた `/` と食い違う）。
        // Windows も `/` を受けるので、書いた区切りをそのまま保つ方が設定として一貫する。
        return format!(
            "{}/{rest}",
            home.to_string_lossy().trim_end_matches(['/', '\\'])
        );
    }
    value.to_string()
}

/// 設定ファイルを読む。
#[must_use]
pub fn load_config() -> ConfigOutcome {
    let Some(path) = config_path() else {
        return ConfigOutcome::Missing {
            path: std::path::PathBuf::from("~/.berth/config.toml"),
        };
    };
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigOutcome::Missing { path },
        Err(e) => ConfigOutcome::Broken {
            path,
            error: e.to_string(),
        },
        Ok(text) => match parse_config(&text, dirs::home_dir().as_deref()) {
            Ok(config) => ConfigOutcome::Loaded { path, config },
            Err(error) => ConfigOutcome::Broken { path, error },
        },
    }
}
