mod remote;

pub use remote::GitRemoteRepo;

use crate::cli;
use crate::env::DeltaEnv;
use gix::bstr::ByteSlice;
use gix::config::{
    file::{includes, init, Metadata},
    File, Source,
};
use regex::Regex;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::Path;

use lazy_static::lazy_static;

#[cfg_attr(test, derive(Clone))]
pub struct GitConfig {
    config: Config,
    config_from_env_var: HashMap<String, String>,
    pub enabled: bool,
    remote_url: OnceCell<Option<GitRemoteRepo>>,
}

#[cfg_attr(test, derive(Clone))]
enum Config {
    #[cfg(not(test))]
    Repository(Box<gix::Repository>),
    File(Box<File>),
}

fn read_config(mut paths: impl Iterator<Item = Metadata>) -> gix::Result<File> {
    let home = gix::path::env::home_dir();
    let options = init::Options {
        includes: includes::Options::follow_without_conditional(home.as_deref()),
        lossy: true,
        ..Default::default()
    };
    File::from_paths_metadata_buf(&mut paths, &mut Vec::new(), false, options)
        .map(Option::unwrap_or_default)
}

#[cfg(not(test))]
fn open(dir: Option<&Path>) -> Option<Config> {
    let mut permissions = gix::open::Permissions::default();
    // Delta handles the command-line overrides from its own environment snapshot.
    permissions.config.env = false;
    let options = gix::open::Options::default()
        .permissions(permissions)
        .bail_if_untrusted(true)
        .lossy_config(true);
    if let Some(repo) = dir.and_then(|dir| options.discover(dir).ok()) {
        return Some(Config::Repository(Box::new(repo)));
    }
    // Match the configuration sources enabled when opening a repository.
    let paths = [Source::System, Source::Git, Source::User]
        .iter()
        .filter_map(|&source| {
            source
                .storage_location(&mut gix::path::env::var)
                .map(|path| Metadata::from(source).at(path))
        });
    read_config(paths)
        .ok()
        .map(|config| Config::File(Box::new(config)))
}

impl GitConfig {
    #[cfg(not(test))]
    pub fn try_create(env: &DeltaEnv) -> Option<Self> {
        Some(Self {
            config: open(env.current_dir.as_deref())?,
            config_from_env_var: parse_config_from_env_var(env),
            enabled: true,
            remote_url: OnceCell::new(),
        })
    }

    #[cfg(test)]
    pub fn try_create(_env: &DeltaEnv) -> Option<Self> {
        // Do not read local git configs when testing
        None
    }

    #[cfg(test)]
    pub fn for_testing() -> Option<Self> {
        Some(GitConfig {
            config: Config::File(Box::default()),
            config_from_env_var: HashMap::new(),
            enabled: true,
            remote_url: OnceCell::new(),
        })
    }

    pub fn from_path(env: &DeltaEnv, path: &Path, honor_env_var: bool) -> Self {
        use crate::fatal;

        let config = read_config(std::iter::once(Metadata::from(Source::Local).at(path)))
            .unwrap_or_else(|err| fatal(format!("Failed to read git config: {err:#}")));
        Self {
            config: Config::File(Box::new(config)),
            config_from_env_var: if honor_env_var {
                parse_config_from_env_var(env)
            } else {
                HashMap::new()
            },
            enabled: true,
            remote_url: OnceCell::new(),
        }
    }

    fn with_config<T>(&self, f: impl FnOnce(&File) -> T) -> T {
        match &self.config {
            #[cfg(not(test))]
            Config::Repository(repo) => f(repo.config_snapshot().plumbing()),
            Config::File(config) => f(config),
        }
    }

    pub fn get<T>(&self, key: &str) -> Option<T>
    where
        T: GitConfigGet,
    {
        if self.enabled {
            T::git_config_get(key, self)
        } else {
            None
        }
    }

    #[cfg(test)]
    fn get_remote_url_impl(&self) -> Option<GitRemoteRepo> {
        GitRemoteRepo::for_testing()
    }

    #[cfg(not(test))]
    fn get_remote_url_impl(&self) -> Option<GitRemoteRepo> {
        let Config::Repository(repo) = &self.config else {
            return None;
        };
        let remote = repo.find_remote("origin").ok()?;
        let url = remote.url(gix::remote::Direction::Fetch)?.to_bstring();
        url.to_str().ok()?.parse().ok()
    }

    pub fn get_remote_url(&self) -> &Option<GitRemoteRepo> {
        self.remote_url.get_or_init(|| self.get_remote_url_impl())
    }

    pub fn theme_names(&self) -> Vec<String> {
        self.with_config(|config| {
            config
                .sections_by_name("delta")
                .into_iter()
                .flatten()
                .filter(|section| {
                    section.contains_value_name("light") || section.contains_value_name("dark")
                })
                .filter_map(|section| {
                    let name = section.header().subsection_name()?.to_str().ok()?;
                    (!name.is_empty()).then(|| name.to_owned())
                })
                .collect()
        })
    }

    #[cfg(feature = "gitu")]
    pub fn section_values(&self, subsection: Option<&str>, key: &str) -> Vec<String> {
        let subsection = subsection.map(|name| name.as_bytes().as_bstr());
        self.with_config(|config| {
            config
                .strings_by("delta", subsection, key)
                .into_iter()
                .flatten()
                .filter_map(|value| String::from_utf8(value.into()).ok())
                .collect()
        })
    }
}

fn parse_config_from_env_var(env: &DeltaEnv) -> HashMap<String, String> {
    if let Some(s) = &env.git_config_parameters {
        parse_config_from_env_var_value(s)
    } else {
        HashMap::new()
    }
}

lazy_static! {
    static ref GIT_CONFIG_PARAMETERS_REGEX: Regex = Regex::new(
        r"(?x)
        (?:                               # Non-capturing group containing union
            '(delta\.[a-z-]+)=([^']+)'    # Git <2.31.0 format
        |
            '(delta\.[a-z-]+)'='([^']+)'  # Git ≥2.31.0 format
        )
        "
    )
    .unwrap();
}

fn parse_config_from_env_var_value(s: &str) -> HashMap<String, String> {
    GIT_CONFIG_PARAMETERS_REGEX
        .captures_iter(s)
        .map(|captures| {
            let (i, j) = match (
                captures.get(1),
                captures.get(2),
                captures.get(3),
                captures.get(4),
            ) {
                (Some(_), Some(_), None, None) => (1, 2),
                (None, None, Some(_), Some(_)) => (3, 4),
                _ => (0, 0),
            };
            if (i, j) == (0, 0) {
                ("".to_string(), "".to_string())
            } else {
                (captures[i].to_string(), captures[j].to_string())
            }
        })
        .collect()
}

pub trait GitConfigGet {
    fn git_config_get(key: &str, git_config: &GitConfig) -> Option<Self>
    where
        Self: Sized;
}

impl GitConfigGet for String {
    fn git_config_get(key: &str, git_config: &GitConfig) -> Option<Self> {
        match git_config.config_from_env_var.get(key) {
            Some(val) => Some(val.to_string()),
            None => {
                let value = git_config
                    .with_config(|config| config.string_implicit(key))?
                    .unwrap_or_default();
                String::from_utf8(value.into()).ok()
            }
        }
    }
}

impl GitConfigGet for Option<String> {
    fn git_config_get(key: &str, git_config: &GitConfig) -> Option<Self> {
        String::git_config_get(key, git_config).map(Some)
    }
}

impl GitConfigGet for bool {
    fn git_config_get(key: &str, git_config: &GitConfig) -> Option<Self> {
        match git_config.config_from_env_var.get(key).map(|s| s.as_str()) {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => git_config.with_config(|config| config.boolean(key).ok().flatten()),
        }
    }
}

impl GitConfigGet for cli::SideBySide {
    fn git_config_get(key: &str, git_config: &GitConfig) -> Option<Self> {
        if let Some(Ok(value)) = git_config.config_from_env_var.get(key).map(|s| s.parse()) {
            return Some(value);
        }
        match git_config.with_config(|config| config.string_implicit(key))? {
            Some(value) => value.to_str().ok()?.parse().ok(),
            None => Some(Self::On(cli::SideBySideScope::All)),
        }
    }
}

impl GitConfigGet for usize {
    fn git_config_get(key: &str, git_config: &GitConfig) -> Option<Self> {
        if let Some(s) = git_config.config_from_env_var.get(key) {
            if let Ok(n) = s.parse::<usize>() {
                return Some(n);
            }
        }
        git_config
            .with_config(|config| config.integer(key).ok().flatten())
            .map(|value| value as usize)
    }
}

impl GitConfigGet for f64 {
    fn git_config_get(key: &str, git_config: &GitConfig) -> Option<Self> {
        if let Some(s) = git_config.config_from_env_var.get(key) {
            if let Ok(n) = s.parse::<f64>() {
                return Some(n);
            }
        }
        git_config
            .with_config(|config| config.string(key))?
            .to_str()
            .ok()?
            .parse()
            .ok()
    }
}

#[cfg(test)]
mod tests {

    use super::parse_config_from_env_var_value;

    #[test]
    fn test_parse_config_from_env_var_value() {
        // To generate test cases, use git -c ... with
        // [core]
        //     pager = env | grep GIT_CONFIG_PARAMETERS

        // We test multiple formats because the format of the value stored by
        // git in this environment variable has changed in recent versions of
        // Git. See
        // https://github.com/git/git/blob/311531c9de557d25ac087c1637818bd2aad6eb3a/Documentation/RelNotes/2.31.0.txt#L127-L130

        for env_var_value in &["'user.name=xxx'", "'user.name'='xxx'"] {
            let config = parse_config_from_env_var_value(env_var_value);
            assert!(config.is_empty());
        }

        for env_var_value in &["'delta.plus-style=green'", "'delta.plus-style'='green'"] {
            let config = parse_config_from_env_var_value(env_var_value);
            assert_eq!(config["delta.plus-style"], "green");
        }

        for env_var_value in &[
            r##"'user.name=xxx' 'delta.hunk-header-line-number-style=red "#067a00"'"##,
            r##"'user.name'='xxx' 'delta.hunk-header-line-number-style'='red "#067a00"'"##,
        ] {
            let config = parse_config_from_env_var_value(env_var_value);
            assert_eq!(
                config["delta.hunk-header-line-number-style"],
                r##"red "#067a00""##
            );
        }

        for env_var_value in &[
            r##"'user.name=xxx' 'delta.side-by-side=false'"##,
            r##"'user.name'='xxx' 'delta.side-by-side'='false'"##,
        ] {
            let config = parse_config_from_env_var_value(env_var_value);
            assert_eq!(config["delta.side-by-side"], "false");
        }

        for env_var_value in &[
            r##"'delta.plus-style=green' 'delta.side-by-side=false' 'delta.hunk-header-line-number-style=red "#067a00"'"##,
            r##"'delta.plus-style'='green' 'delta.side-by-side'='false' 'delta.hunk-header-line-number-style'='red "#067a00"'"##,
        ] {
            let config = parse_config_from_env_var_value(env_var_value);
            assert_eq!(config["delta.plus-style"], "green");
            assert_eq!(config["delta.side-by-side"], "false");
            assert_eq!(
                config["delta.hunk-header-line-number-style"],
                r##"red "#067a00""##
            );
        }
    }
}
