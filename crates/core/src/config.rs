use crate::{Error, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub(crate) struct Config {
    postclone: Vec<Postclone>,
}

impl Config {
    pub(crate) fn load(workspace: &Path) -> Result<Self> {
        let path = workspace.join(".rift.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        parse(&path, &fs::read_to_string(&path)?)
    }

    pub(crate) fn postclone(&self) -> &[Postclone] {
        &self.postclone
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Postclone {
    run: String,
}

impl Postclone {
    pub(crate) fn run(&self) -> &str {
        &self.run
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    #[serde(default)]
    hooks: RawHooks,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHooks {
    #[serde(default)]
    postclone: Vec<RawPostclone>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPostclone {
    run: String,
}

fn parse(path: &Path, contents: &str) -> Result<Config> {
    let raw = toml::from_str::<RawConfig>(contents)
        .map_err(|error| invalid_config(path, error.to_string()))?;
    if raw.version != 1 {
        return Err(invalid_config(
            path,
            format!("unsupported config version {}", raw.version),
        ));
    }
    raw.hooks
        .postclone
        .into_iter()
        .map(|step| {
            let run = step.run.trim().to_owned();
            if run.is_empty() {
                Err(invalid_config(path, "postclone run cannot be empty"))
            } else {
                Ok(Postclone { run })
            }
        })
        .collect::<Result<Vec<_>>>()
        .map(|postclone| Config { postclone })
}

fn invalid_config(path: &Path, message: impl Into<String>) -> Error {
    Error::InvalidConfig {
        path: PathBuf::from(path),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ordered_postclone_steps() {
        let config = parse(
            Path::new(".rift.toml"),
            r#"
version = 1

[[hooks.postclone]]
run = "echo one"

[[hooks.postclone]]
run = "echo two"
"#,
        )
        .unwrap();

        assert_eq!(
            config
                .postclone()
                .iter()
                .map(Postclone::run)
                .collect::<Vec<_>>(),
            vec!["echo one", "echo two"]
        );
    }

    #[test]
    fn rejects_empty_steps() {
        assert!(matches!(
            parse(
                Path::new(".rift.toml"),
                r#"
version = 1

[[hooks.postclone]]
run = " "
"#,
            ),
            Err(Error::InvalidConfig { .. })
        ));
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(matches!(
            parse(
                Path::new(".rift.toml"),
                r#"
version = 1

[[hooks.postclone]]
run = "echo ok"
shell = "sh"
"#,
            ),
            Err(Error::InvalidConfig { .. })
        ));
    }
}
