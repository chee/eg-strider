/// A file that is not a saved document, or is damaged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a saved eg-strider document")
    }
}

impl std::error::Error for Malformed {}
