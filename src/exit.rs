//! Closed exit-code contract for the canary process.

use std::fmt;

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

impl fmt::Display for ExitCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Ok => "ok",
            Self::Config => "config",
            Self::Software => "software",
        };
        write!(formatter, "{name} ({})", self.code())
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

    #[test]
    fn display_names_the_outcome_and_status() {
        assert_eq!(ExitCode::Ok.to_string(), "ok (0)");
        assert_eq!(ExitCode::Config.to_string(), "config (78)");
        assert_eq!(ExitCode::Software.to_string(), "software (70)");
    }
}
