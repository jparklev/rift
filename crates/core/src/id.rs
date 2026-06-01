use std::fmt;
use ulid::Ulid;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RiftId(String);

impl RiftId {
    pub(crate) fn new() -> Self {
        Self(Ulid::new().to_string())
    }

    pub(crate) fn from_stored(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RiftId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
