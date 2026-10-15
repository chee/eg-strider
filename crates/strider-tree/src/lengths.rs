use crate::View;

/// An entry's length in each view: its length where present, else 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Lengths {
    pub prepare: u32,
    pub effect: u32,
}

impl Lengths {
    /// Returns the length in one view.
    pub(crate) fn of(self, view: View) -> u32 {
        match view {
            View::Prepare => self.prepare,
            View::Effect => self.effect,
        }
    }

    /// Adds lengths, wrapping, so that a difference from [`minus`](Self::minus) adds back.
    pub(crate) fn plus(self, other: Lengths) -> Lengths {
        Lengths { prepare: self.prepare.wrapping_add(other.prepare), effect: self.effect.wrapping_add(other.effect) }
    }

    /// Subtracts lengths, wrapping.
    pub(crate) fn minus(self, other: Lengths) -> Lengths {
        Lengths { prepare: self.prepare.wrapping_sub(other.prepare), effect: self.effect.wrapping_sub(other.effect) }
    }
}
