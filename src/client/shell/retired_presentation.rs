//! Withdrawal is client-local: explicit backend/API compatibility remains available.
use super::*;

pub(super) const AVAILABLE: bool = false;

impl ClientShellState {
    // Not a setting or capability: production clients cannot opt back in. Dormant
    // controller tests opt in explicitly without changing the normal constructor.
    pub(super) fn retired_presentation_enabled(&self) -> bool {
        #[cfg(test)]
        {
            self.retired_presentation_fixture
        }
        #[cfg(not(test))]
        {
            AVAILABLE
        }
    }

    pub(super) fn discard_retired_overlay(&mut self) {
        if !self.retired_presentation_enabled()
            && matches!(self.overlay, Some(ClientShellOverlay::Kanban))
        {
            self.overlay = None;
        }
    }
}
