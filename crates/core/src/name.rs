use crate::{Error, Result};
use rand::RngExt;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RiftName(String);

impl RiftName {
    pub(crate) fn from_optional(name: Option<String>) -> Result<Self> {
        Self::new(name.unwrap_or_else(generated_name))
    }

    fn new(name: String) -> Result<Self> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || matches!(name.as_str(), ".rift" | ".trash")
            || Path::new(&name).components().count() != 1
        {
            return Err(Error::Path(format!("invalid rift name: {name}")));
        }
        Ok(Self(name))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// A fresh readable name for automatic destination selection. Callers
    /// still need to check the filesystem; the word list intentionally keeps
    /// the normal name space compact and pleasant rather than pretending it is
    /// globally unique.
    pub(crate) fn generated() -> Self {
        Self(generated_name())
    }

    /// Retain the readable stem while adding a collision-resistant fallback.
    pub(crate) fn generated_with_suffix(suffix: &str) -> Self {
        Self(format!(
            "{}-{}",
            generated_name(),
            suffix.to_ascii_lowercase()
        ))
    }
}

fn generated_name() -> String {
    const ADJECTIVES: &[&str] = &[
        "amber", "bold", "brisk", "calm", "cedar", "clear", "cobalt", "coral", "dawn", "ember",
        "gentle", "golden", "jade", "lively", "lunar", "mellow", "misty", "noble", "quiet",
        "rapid", "river", "silver", "solar", "spruce", "steady", "swift", "tidal", "verdant",
        "violet", "warm", "wild", "winter",
    ];
    const NOUNS: &[&str] = &[
        "badger", "brook", "canyon", "cedar", "comet", "dune", "falcon", "field", "forest",
        "harbor", "heron", "island", "lantern", "maple", "meadow", "mesa", "otter", "peak", "pine",
        "reef", "ridge", "robin", "sparrow", "summit", "thicket", "trail", "valley", "willow",
        "wren", "yarrow", "zephyr", "fox",
    ];

    let mut rng = rand::rng();
    format!(
        "{}-{}",
        ADJECTIVES[rng.random_range(0..ADJECTIVES.len())],
        NOUNS[rng.random_range(0..NOUNS.len())]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_single_path_segments() {
        assert_eq!(RiftName::new("child".into()).unwrap().as_str(), "child");
        assert!(RiftName::new(String::new()).is_err());
        assert!(RiftName::new(".".into()).is_err());
        assert!(RiftName::new("..".into()).is_err());
        assert!(RiftName::new(".rift".into()).is_err());
        assert!(RiftName::new(".trash".into()).is_err());
        assert!(RiftName::new("parent/child".into()).is_err());
    }

    #[test]
    fn generated_names_are_readable_segments() {
        let name = RiftName::from_optional(None).unwrap();
        let parts = name.as_str().split('-').collect::<Vec<_>>();

        assert_eq!(parts.len(), 2);
        assert!(
            parts
                .iter()
                .all(|part| part.chars().all(|character| character.is_ascii_lowercase()))
        );
    }
}
