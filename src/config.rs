//! The validated runtime plan assembled once from the only two external inputs.

use std::env::VarError;
use std::fmt;

use crate::log::{LogFilter, Record};
use crate::port::{PortError, PortRequest};

/// The configuration the process needs before it binds a listener.
///
/// `PORT` is the only fatal input: `PortRequest::from_env` rejects a missing,
/// non-numeric, or out-of-range value with [`ConfigError::Port`]. `RUST_LOG` is
/// bounded and non-fatal: `LogFilter::parse` recognises exactly `off`, `error`,
/// `warn`, and `info`, defaults an absent value to [`LogFilter::Info`] with no
/// notice, and defaults any other supplied value to [`LogFilter::Info`] together
/// with [`Record::FilterUnrecognised`], which the plan carries as its
/// [`RuntimePlan::notice`]. Only an absent variable is silent: a supplied value
/// that names no severity earns the notice, and a value that is not valid
/// Unicode text cannot name one, so it earns the notice too.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimePlan {
    /// The port to bind, where zero asks the operating system for one.
    pub listen: PortRequest,
    /// The severity filter every record is written through.
    pub filter: LogFilter,
    /// The record the filter choice requires, present only when `RUST_LOG`
    /// supplied an unrecognised value.
    pub notice: Option<Record>,
}

impl RuntimePlan {
    /// Reads `PORT` and `RUST_LOG` once each and validates them together.
    ///
    /// These are the only external inputs of the process: `PORT` is fatal and
    /// `RUST_LOG` is bounded and non-fatal, so a rejected filter never stops the
    /// service and a missing or malformed port always does. The two inputs are
    /// read through one seam each, so the plan tests exercise the same
    /// translation of an environment answer that the process does.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Port`] when `PORT` is absent, not valid Unicode
    /// text, or not a decimal value in `0..=65535`.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_inputs(
            PortRequest::from_env(),
            parse_var(std::env::var("RUST_LOG")).as_deref(),
        )
    }

    /// Pairs the port decision with the filter decision and its notice.
    fn from_inputs(
        listen: Result<PortRequest, PortError>,
        rust_log: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let listen = listen.map_err(ConfigError::Port)?;
        let (filter, notice) = LogFilter::parse(rust_log).into_filter_and_notice();
        Ok(Self {
            listen,
            filter,
            notice,
        })
    }
}

/// Reads `RUST_LOG` once and reduces the three environment outcomes to the two
/// the filter distinguishes.
///
/// Only an absent variable reaches the filter as `None`, because only an absent
/// variable is the normal boot. A value that is not valid Unicode text is still
/// a value the operator supplied, and it cannot name a severity, so it is
/// examined as the empty text, which names none: it earns the same notice as
/// every other unrecognised value instead of passing for silence.
fn parse_var(raw: Result<String, VarError>) -> Option<String> {
    match raw {
        Ok(value) => Some(value),
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(_)) => Some(String::new()),
    }
}

/// Why the runtime plan could not be assembled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// The fatal `PORT` input was absent or rejected.
    Port(PortError),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Port(error) => write!(formatter, "PORT: {error}"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::{ConfigError, RuntimePlan, parse_var};
    use crate::log::{Level, LogFilter, Record};
    use crate::port::{Port, PortError, PortRequest};

    #[test]
    fn absent_rust_log_defaults_the_filter_without_a_notice() {
        let plan = RuntimePlan::from_inputs(Ok(PortRequest::Ephemeral), None).unwrap();
        assert_eq!(plan.listen, PortRequest::Ephemeral);
        assert_eq!(plan.filter, LogFilter::Info);
        assert_eq!(plan.notice, None);
    }

    #[test]
    fn a_text_value_and_an_absent_variable_stay_distinct() {
        assert_eq!(parse_var(Ok("info".to_owned())).as_deref(), Some("info"));
        assert_eq!(parse_var(Err(std::env::VarError::NotPresent)), None);
    }

    #[test]
    fn a_non_unicode_rust_log_earns_the_notice_rather_than_silence() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let invalid = std::env::VarError::NotUnicode(OsString::from_vec(vec![0xff]));
        let plan = RuntimePlan::from_inputs(
            Ok(PortRequest::Ephemeral),
            parse_var(Err(invalid)).as_deref(),
        )
        .unwrap();
        assert_eq!(plan.filter, LogFilter::Info);
        assert_eq!(plan.notice, Some(Record::FilterUnrecognised));
    }

    #[test]
    fn an_unrecognised_rust_log_defaults_the_filter_with_one_warn_notice() {
        let plan = RuntimePlan::from_inputs(Ok(PortRequest::Ephemeral), Some("garbage")).unwrap();
        assert_eq!(plan.filter, LogFilter::Info);
        assert_eq!(plan.notice, Some(Record::FilterUnrecognised));
        assert_eq!(plan.notice.map(Record::level), Some(Level::Warn));
    }

    #[test]
    fn accepted_filter_yields_no_notice() {
        let plan = RuntimePlan::from_inputs(Ok(PortRequest::Ephemeral), Some("off")).unwrap();
        assert_eq!(plan.listen, PortRequest::Ephemeral);
        assert_eq!(plan.filter, LogFilter::Off);
        assert_eq!(plan.notice, None);
    }

    #[test]
    fn rejected_port_is_the_only_plan_error() {
        assert_eq!(
            RuntimePlan::from_inputs(Err(PortError::Missing), Some("info")),
            Err(ConfigError::Port(PortError::Missing))
        );
    }

    #[test]
    fn fixed_port_and_filter_travel_into_the_plan() {
        let plan = RuntimePlan::from_inputs(
            Ok(PortRequest::Fixed(Port::parse("8080").unwrap())),
            Some("warn"),
        )
        .unwrap();
        assert_eq!(plan.listen.port(), 8080);
        assert_eq!(plan.filter, LogFilter::Warn);
        assert_eq!(plan.notice, None);
    }

    #[test]
    fn config_error_names_the_port_rejection() {
        let error = ConfigError::Port(PortError::OutOfRange);
        assert_eq!(
            error.to_string(),
            "PORT: PORT must be 1..=65535, or 0 for an ephemeral port"
        );
    }
}
