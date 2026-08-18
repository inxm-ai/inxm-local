//! Shared classification for "did we get the thing, is it just not there, or
//! did something actually go wrong reading it".
//!
//! A recurring bug shape across this codebase was collapsing all three into
//! one `Err` branch: a missing chat session, a missing repair journal entry,
//! and a closed stdin were all treated the same as "nothing here, proceed
//! with a default" — even when the real cause was a corrupt file or a closed
//! stream that must not be silently papered over. `Presence` forces callers
//! to name the third case instead of falling into it by accident.

/// The outcome of trying to read something that might legitimately not exist.
///
/// `Absent` and `Broken` must never be handled the same way: `Absent` means
/// it is safe to proceed as if starting fresh (or, for a stream, that it has
/// definitively ended). `Broken` means the caller could not tell one way or
/// the other and must not guess.
pub enum Presence<T, E = std::io::Error> {
    Found(T),
    Absent,
    Broken(E),
}

impl<T> Presence<T, std::io::Error> {
    /// Classify a filesystem read. `NotFound` is a legitimate "nothing here
    /// yet"; every other I/O error (permissions, corrupt data mapped to
    /// `InvalidData`, ...) is not, and must surface instead of being read as
    /// an absence.
    pub fn from_io_result(result: std::io::Result<T>) -> Self {
        match result {
            Ok(value) => Presence::Found(value),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Presence::Absent,
            Err(err) => Presence::Broken(err),
        }
    }
}
