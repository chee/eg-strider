mod lengths;
mod node;
mod prepare_effect_tree;
mod view;

pub use lengths::Lengths;
pub use prepare_effect_tree::{Cursor, PrepareEffectTree};
pub use view::View;

/// Id of an entry, handed out from 0 in order of creation.
pub type EntryId = u32;
