//! This mod doesn't actually contain any driver, but meant to provide some
//! common op type and utilities for unix platform (for iour and polling).

pub(crate) mod op;

use crate::RawFd;

/// The overlapped struct for unix needn't contain extra fields.
pub(crate) struct Overlapped;

impl Overlapped {
    pub fn new(_driver: RawFd) -> Self {
        Self
    }
}
