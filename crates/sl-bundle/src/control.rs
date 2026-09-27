use crate::{Error, Result};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Extract,
    Copy,
    Patch,
    Sign,
    Pack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    pub completed: u64,
    pub total: u64,
}

/// Callbacks may run on extraction workers. Consumers should retain the maximum
/// completed value observed for each phase rather than assuming delivery order.
#[derive(Default, Clone, Copy)]
pub struct Control<'a> {
    pub is_cancelled: Option<&'a (dyn Fn() -> bool + Sync)>,
    pub on_progress: Option<&'a (dyn Fn(Progress) + Sync)>,
}

impl fmt::Debug for Control<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Control")
            .field("cancellation", &self.is_cancelled.is_some())
            .field("progress", &self.on_progress.is_some())
            .finish()
    }
}

impl Control<'_> {
    pub fn check(self) -> Result<()> {
        if self.is_cancelled.is_some_and(|cancelled| cancelled()) {
            return Err(Error::Cancelled);
        }

        Ok(())
    }

    pub(crate) fn report(self, phase: Phase, completed: u64, total: u64) {
        if let Some(callback) = self.on_progress {
            callback(Progress { phase, completed, total });
        }
    }
}
