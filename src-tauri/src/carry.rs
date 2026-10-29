//! A file drag carried across the edge with the cursor. The files travel as a transfer batch while the
//! button stays held; on this side they become a native drag under the cursor once both the held button
//! (Drag frame / return) and every file (batch complete) are here, in either order. Released before the
//! files arrived: they're saved to Downloads/LOKA instead.

use std::{collections::HashMap, path::PathBuf};

#[derive(Debug, PartialEq)]
pub enum Step {
    /// start a native drag of these files now
    Start(Vec<PathBuf>),
    /// the button went up first: keep the files as a plain transfer
    Save(Vec<PathBuf>),
    Wait,
}

enum State {
    /// button held here, files on their way
    Waiting,
    /// files here, the cursor (and its held button) not yet
    Ready(Vec<PathBuf>),
    /// button released before the files arrived
    Released,
}

#[derive(Default)]
pub struct Carry(HashMap<u64, State>);

impl Carry {
    /// The cursor arrived here with drag `batch` and its button held.
    pub fn held(&mut self, batch: u64) -> Step {
        match self.0.remove(&batch) {
            Some(State::Ready(files)) => Step::Start(files),
            _ => {
                self.0.insert(batch, State::Waiting);
                Step::Wait
            }
        }
    }

    /// Every file of drag `batch` arrived.
    pub fn files(&mut self, batch: u64, files: Vec<PathBuf>, button_down: bool) -> Step {
        match self.0.remove(&batch) {
            Some(State::Waiting) if button_down => Step::Start(files),
            Some(State::Waiting | State::Released) => Step::Save(files),
            Some(State::Ready(_)) | None => {
                self.0.insert(batch, State::Ready(files));
                Step::Wait
            }
        }
    }

    /// The button is up: drags still waiting for their files end up saved.
    pub fn released(&mut self) {
        for s in self.0.values_mut() {
            if matches!(s, State::Waiting) {
                *s = State::Released;
            }
        }
    }

    pub fn waiting(&self) -> bool {
        self.0.values().any(|s| matches!(s, State::Waiting))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_starts_whichever_comes_last_and_saves_after_an_early_release() {
        let f = || vec![PathBuf::from("a.txt")];
        let mut c = Carry::default();
        // files first (tiny files beat the Drag frame), then the held cursor
        assert_eq!(c.files(1, f(), false), Step::Wait);
        assert_eq!(c.held(1), Step::Start(f()));
        // held cursor first, files later with the button still down
        assert_eq!(c.held(2), Step::Wait);
        assert!(c.waiting());
        assert_eq!(c.files(2, f(), true), Step::Start(f()));
        // released while a big file was still on its way
        assert_eq!(c.held(3), Step::Wait);
        c.released();
        assert!(!c.waiting());
        assert_eq!(c.files(3, f(), false), Step::Save(f()));
    }
}
