//! Listener-port domain values parsed from the fatal `PORT` input.

use std::fmt;

/// A fixed TCP port number in `1..=65535`.
///
/// The wrapped value is private so every instance has passed the range check;
/// construct one through [`Port::parse`] or [`Port::try_from`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Port(u16);

impl Port {
    /// Decodes an ASCII-decimal port in `1..=65535`.
    ///
    /// Only the digits `0`-`9` are accepted. Signs, whitespace, underscores,
    /// and hexadecimal prefixes are rejected as [`PortError::NonNumeric`].
    /// Leading zeros are permitted, so the length of the digit run carries no
    /// meaning on its own: only the decoded decimal value is constrained.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::Empty`] for an empty value,
    /// [`PortError::NonNumeric`] for any non-digit byte, and
    /// [`PortError::OutOfRange`] when the decoded decimal value is zero or
    /// exceeds `65535`.
    pub fn parse(raw: &str) -> Result<Self, PortError> {
        let bytes = raw.as_bytes();
        if bytes.is_empty() {
            return Err(PortError::Empty);
        }
        if !bytes.iter().all(u8::is_ascii_digit) {
            return Err(PortError::NonNumeric);
        }
        let value = raw.parse::<u16>().map_err(|_| PortError::OutOfRange)?;
        Self::try_from(value)
    }
}

impl TryFrom<u16> for Port {
    type Error = PortError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Err(PortError::OutOfRange),
            _ => Ok(Self(value)),
        }
    }
}

impl From<Port> for u16 {
    fn from(port: Port) -> Self {
        port.0
    }
}

/// The listener port requested by the environment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListenPort {
    /// Ask the operating system for a free port.
    Ephemeral,
    /// Bind the named fixed port.
    Fixed(Port),
}

impl ListenPort {
    /// Resolves a raw `PORT` value, where `None` means the variable is unset.
    ///
    /// The decimal value zero, in any zero-padded spelling such as `"0"`,
    /// `"00"`, or `"00000"`, selects [`ListenPort::Ephemeral`]; every other
    /// value must parse as a fixed [`Port`].
    ///
    /// # Errors
    ///
    /// Returns [`PortError::Missing`] when no value is supplied and otherwise
    /// the [`PortError`] reported by [`Port::parse`].
    pub fn parse(raw: Option<&str>) -> Result<Self, PortError> {
        match raw {
            None => Err(PortError::Missing),
            Some(value) if is_decimal_zero(value) => Ok(Self::Ephemeral),
            Some(value) => Ok(Self::Fixed(Port::parse(value)?)),
        }
    }

    /// Reads `PORT` from the process environment, the only fatal external input.
    ///
    /// # Errors
    ///
    /// Returns [`PortError::Missing`] when `PORT` is absent,
    /// [`PortError::NotUnicode`] when it is set but not valid Unicode text, and
    /// otherwise the [`PortError`] reported by [`Self::parse`].
    pub fn from_env() -> Result<Self, PortError> {
        Self::parse_var(std::env::var("PORT"))
    }

    fn parse_var(raw: Result<String, std::env::VarError>) -> Result<Self, PortError> {
        match raw {
            Ok(value) => Self::parse(Some(&value)),
            Err(std::env::VarError::NotPresent) => Err(PortError::Missing),
            Err(std::env::VarError::NotUnicode(_)) => Err(PortError::NotUnicode),
        }
    }

    /// The port to bind, where zero asks the operating system for one.
    #[must_use]
    pub fn port(self) -> u16 {
        match self {
            Self::Ephemeral => 0,
            Self::Fixed(port) => u16::from(port),
        }
    }
}

/// Why a raw listener-port value was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortError {
    /// `PORT` is absent from the environment.
    Missing,
    /// `PORT` is present but is not valid Unicode text.
    NotUnicode,
    /// `PORT` is present but has no characters.
    Empty,
    /// `PORT` is not made only of ASCII decimal digits.
    NonNumeric,
    /// `PORT` is decimal zero or greater than `65535`.
    OutOfRange,
}

impl fmt::Display for PortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Missing => "PORT is not set",
            Self::NotUnicode => "PORT is not valid Unicode text",
            Self::Empty => "PORT is empty",
            Self::NonNumeric => "PORT is not an ASCII decimal integer",
            Self::OutOfRange => "PORT must be 1..=65535, or 0 for an ephemeral port",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PortError {}

fn is_decimal_zero(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte == b'0')
}

#[cfg(test)]
mod tests {
    use super::{ListenPort, Port, PortError};

    fn fixed(raw: &str) -> ListenPort {
        ListenPort::Fixed(Port::parse(raw).unwrap())
    }

    #[test]
    fn raw_zero_is_ephemeral() {
        assert_eq!(ListenPort::parse(Some("0")), Ok(ListenPort::Ephemeral));
    }

    #[test]
    fn zero_padded_zero_is_ephemeral() {
        assert_eq!(ListenPort::parse(Some("00000")), Ok(ListenPort::Ephemeral));
    }

    #[test]
    fn six_digit_zero_is_ephemeral() {
        assert_eq!(ListenPort::parse(Some("000000")), Ok(ListenPort::Ephemeral));
    }

    #[test]
    fn lowest_fixed_port_is_accepted() {
        assert_eq!(ListenPort::parse(Some("1")), Ok(fixed("1")));
    }

    #[test]
    fn highest_fixed_port_is_accepted() {
        assert_eq!(ListenPort::parse(Some("65535")), Ok(fixed("65535")));
    }

    #[test]
    fn port_above_range_is_rejected() {
        assert_eq!(ListenPort::parse(Some("65536")), Err(PortError::OutOfRange));
    }

    #[test]
    fn overlong_digit_run_is_out_of_range() {
        assert_eq!(
            ListenPort::parse(Some("123456")),
            Err(PortError::OutOfRange)
        );
        assert_eq!(ListenPort::parse(Some("99999")), Err(PortError::OutOfRange));
    }

    #[test]
    fn zero_padded_fixed_port_decodes_to_its_decimal_value() {
        assert_eq!(ListenPort::parse(Some("000080")), Ok(fixed("80")));
    }

    #[test]
    fn non_ascii_digits_are_non_numeric() {
        assert_eq!(ListenPort::parse(Some("８０")), Err(PortError::NonNumeric));
    }

    #[test]
    fn negative_value_is_non_numeric() {
        assert_eq!(ListenPort::parse(Some("-1")), Err(PortError::NonNumeric));
    }

    #[test]
    fn empty_value_is_empty() {
        assert_eq!(ListenPort::parse(Some("")), Err(PortError::Empty));
    }

    #[test]
    fn leading_space_is_non_numeric() {
        assert_eq!(ListenPort::parse(Some(" 80")), Err(PortError::NonNumeric));
    }

    #[test]
    fn plus_sign_is_non_numeric() {
        assert_eq!(ListenPort::parse(Some("+80")), Err(PortError::NonNumeric));
    }

    #[test]
    fn trailing_junk_is_non_numeric() {
        assert_eq!(ListenPort::parse(Some("80x")), Err(PortError::NonNumeric));
    }

    #[test]
    fn hex_prefix_is_non_numeric() {
        assert_eq!(ListenPort::parse(Some("0x50")), Err(PortError::NonNumeric));
    }

    #[test]
    fn underscore_is_non_numeric() {
        assert_eq!(ListenPort::parse(Some("8_0")), Err(PortError::NonNumeric));
    }

    #[test]
    fn leading_zeroes_are_decimal() {
        assert_eq!(ListenPort::parse(Some("0080")), Ok(fixed("80")));
    }

    #[test]
    fn missing_value_is_missing() {
        assert_eq!(ListenPort::parse(None), Err(PortError::Missing));
    }

    #[test]
    fn absent_variable_reports_missing() {
        assert_eq!(
            ListenPort::parse_var(Err(std::env::VarError::NotPresent)),
            Err(PortError::Missing)
        );
    }

    #[test]
    fn unicode_value_parses_like_any_other() {
        assert_eq!(
            ListenPort::parse_var(Ok("0080".to_owned())),
            Ok(fixed("80"))
        );
    }

    #[test]
    fn non_unicode_variable_reports_not_unicode() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let invalid = std::env::VarError::NotUnicode(OsString::from_vec(vec![0x80]));
        assert_eq!(
            ListenPort::parse_var(Err(invalid)),
            Err(PortError::NotUnicode)
        );
    }

    #[test]
    fn try_from_rejects_zero() {
        assert_eq!(Port::try_from(0), Err(PortError::OutOfRange));
    }

    #[test]
    fn try_from_accepts_fixed_port_and_round_trips() {
        let port = Port::try_from(8080).unwrap();
        assert_eq!(u16::from(port), 8080);
    }

    #[test]
    fn ephemeral_binds_port_zero() {
        assert_eq!(ListenPort::Ephemeral.port(), 0);
    }

    #[test]
    fn fixed_binds_the_parsed_port() {
        assert_eq!(fixed("8080").port(), 8080);
    }

    #[test]
    fn display_names_every_error() {
        assert_eq!(PortError::Missing.to_string(), "PORT is not set");
        assert_eq!(
            PortError::NotUnicode.to_string(),
            "PORT is not valid Unicode text"
        );
        assert_eq!(PortError::Empty.to_string(), "PORT is empty");
        assert_eq!(
            PortError::NonNumeric.to_string(),
            "PORT is not an ASCII decimal integer"
        );
        assert_eq!(
            PortError::OutOfRange.to_string(),
            "PORT must be 1..=65535, or 0 for an ephemeral port"
        );
    }
}
