//! Closed exit-code contract for the canary process.

/// The process status this service may terminate with.
///
/// Codes 74 (`EX_IOERR`, readiness failure) and 75 (`EX_TEMPFAIL`) are
/// deliberately never emitted: the service has no readiness dependency, and
/// lost log records are counted rather than promoted to a fatal outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitCode {
    /// Clean shutdown.
    Ok,
    /// External configuration was rejected.
    Config,
    /// An internal invariant or runtime step failed.
    Software,
}

impl ExitCode {
    /// The numeric process status for this outcome.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Config => 78,
            Self::Software => 70,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ExitCode;

    #[test]
    fn ok_is_zero() {
        assert_eq!(ExitCode::Ok.code(), 0);
    }

    #[test]
    fn config_is_ex_config() {
        assert_eq!(ExitCode::Config.code(), 78);
    }

    #[test]
    fn software_is_ex_software() {
        assert_eq!(ExitCode::Software.code(), 70);
    }
}
