//! Bounded HTTP/1.1 request-head parsing and the startup-built reply plan.
//!
//! The parser inspects at most [`MAX_HEAD_BYTES`] of head bytes and never
//! allocates. [`ResponsePlan`] renders five reply buffers once from the build
//! version; they serve the seven reply outcomes (200 and 404, each in a
//! `Full` and a `HeadOnly` form, plus 405, 400, and 431), so the connection
//! write path only borrows bytes and never formats.

/// The largest request head the parser classifies, in bytes.
///
/// Callers feed at most this many bytes; a full window of this size without a
/// head terminator is [`ParseOutcome::TooLarge`].
pub const MAX_HEAD_BYTES: usize = 1024;

/// The largest reply the plan renders, in bytes.
///
/// Every planned reply is a fixed array of this size with a shorter live
/// length, so no rendered reply is longer than this bound. The serving write
/// path asserts its per-turn write budget covers one whole reply.
pub const MAX_REPLY_BYTES: usize = 512;

/// The largest version text a liveness body accepts, in bytes.
pub const MAX_VERSION_BYTES: usize = 128;

/// The digits a `usize` value may need in decimal.
const DECIMAL_DIGITS: usize = 20;

const DECIMAL_TABLE: &[u8; 10] = b"0123456789";

const STATUS_OK: &[u8] = b"HTTP/1.1 200 OK\r\n";
const STATUS_NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\n";
const STATUS_METHOD_NOT_ALLOWED: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\n";
const STATUS_BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\n";
const STATUS_TOO_LARGE: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\n";
const CONTENT_TYPE: &[u8] = b"Content-Type: text/plain; charset=utf-8\r\n";
const CONTENT_LENGTH: &[u8] = b"Content-Length: ";
const CONNECTION_CLOSE: &[u8] = b"Connection: close\r\n";
const ALLOW: &[u8] = b"Allow: GET, HEAD\r\n";
const CRLF: &[u8] = b"\r\n";

const LIVENESS_PREFIX: &[u8] = b"Canary ";
const LIVENESS_SUFFIX: &[u8] = b" is alive!\r\n";
const NOT_FOUND_BODY: &[u8] = b"Not found\r\n";
const METHOD_NOT_ALLOWED_BODY: &[u8] = b"Method not allowed\r\n";
const BAD_REQUEST_BODY: &[u8] = b"Bad request\r\n";
const TOO_LARGE_BODY: &[u8] = b"Request head too large\r\n";

/// The longest liveness body the plan accepts, in bytes.
const LIVENESS_MAX_BYTES: usize = LIVENESS_PREFIX.len() + MAX_VERSION_BYTES + LIVENESS_SUFFIX.len();

/// The digits a planned `Content-Length` value may need.
const MAX_LENGTH_DIGITS: usize = decimal_len(LIVENESS_MAX_BYTES);

const _: () = assert!(planned_bytes(STATUS_OK, &[], LIVENESS_MAX_BYTES) <= MAX_REPLY_BYTES);
const _: () =
    assert!(planned_bytes(STATUS_NOT_FOUND, &[], NOT_FOUND_BODY.len()) <= MAX_REPLY_BYTES);
const _: () = assert!(
    planned_bytes(
        STATUS_METHOD_NOT_ALLOWED,
        ALLOW,
        METHOD_NOT_ALLOWED_BODY.len()
    ) <= MAX_REPLY_BYTES
);
const _: () =
    assert!(planned_bytes(STATUS_BAD_REQUEST, &[], BAD_REQUEST_BODY.len()) <= MAX_REPLY_BYTES);
const _: () =
    assert!(planned_bytes(STATUS_TOO_LARGE, &[], TOO_LARGE_BODY.len()) <= MAX_REPLY_BYTES);

/// The request method classes the reply plan distinguishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    /// A `GET` token.
    Get,
    /// A `HEAD` token, answered with the headers of the matching `GET`.
    Head,
    /// Every other method token, answered with `405` and an `Allow` header.
    Other,
}

/// The request target classes the reply plan distinguishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    /// The target bytes are exactly `/`.
    Root,
    /// Every other target, including query, absolute-form, and traversal forms.
    Other,
}

/// A request line that parsed completely.
///
/// The request version is validated but not retained: every reply is
/// `HTTP/1.1` and closes the connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    method: Method,
    target: Target,
}

impl Request {
    /// The request method class.
    #[must_use]
    pub const fn method(self) -> Method {
        self.method
    }

    /// The exact-match request target class.
    #[must_use]
    pub const fn target(self) -> Target {
        self.target
    }
}

/// Why a request line was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MalformedKind {
    /// The method token is missing or holds a byte outside the token set.
    MethodToken,
    /// The target token is missing or holds a byte outside the visible range.
    TargetToken,
    /// The version token is missing or is not `HTTP/1.0` or `HTTP/1.1`.
    VersionToken,
    /// A line break in the head is not the `\r\n` pair.
    MissingTerminator,
}

/// The classification of the request-head bytes read so far.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseOutcome {
    /// More bytes are needed before the head can be classified.
    ///
    /// At end of input an `Incomplete` head is a client error, never a wait:
    /// the caller answers `400`.
    Incomplete,
    /// The head ended and its request line is valid.
    Complete(Request),
    /// The head ended but its request line is invalid.
    Malformed(MalformedKind),
    /// The head cannot be terminated within [`MAX_HEAD_BYTES`] bytes.
    TooLarge,
}

/// Classifies the request-head bytes read so far.
///
/// The classification depends only on the bytes, never on how a read was
/// split: callers pass at most [`MAX_HEAD_BYTES`] bytes, and a longer slice is
/// classified from its leading window. A full window with no head terminator
/// is [`ParseOutcome::TooLarge`], while a malformed line inside the window wins
/// over that size verdict.
///
/// Only the request line is parsed; every header line after it is ignored
/// beyond the `\r\n` discipline that terminates the head. A target is
/// [`Target::Root`] only when its bytes are exactly `/`.
#[must_use]
pub fn parse_head(bytes: &[u8]) -> ParseOutcome {
    let window = &bytes[..bytes.len().min(MAX_HEAD_BYTES)];
    match scan_head(window) {
        Scan::Line(end) => match parse_request_line(&window[..end]) {
            Ok(request) => ParseOutcome::Complete(request),
            Err(kind) => ParseOutcome::Malformed(kind),
        },
        Scan::Malformed(kind) => ParseOutcome::Malformed(kind),
        Scan::Incomplete if window.len() == MAX_HEAD_BYTES => ParseOutcome::TooLarge,
        Scan::Incomplete => ParseOutcome::Incomplete,
    }
}

/// The terminator scan outcome for one read of head bytes.
enum Scan {
    Incomplete,
    Malformed(MalformedKind),
    Line(usize),
}

fn scan_head(bytes: &[u8]) -> Scan {
    let mut index = 0;
    let mut first_cr = None;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => return Scan::Malformed(MalformedKind::MissingTerminator),
            b'\r' => {
                match bytes.get(index + 1) {
                    None => return Scan::Incomplete,
                    Some(byte) if *byte != b'\n' => {
                        return Scan::Malformed(MalformedKind::MissingTerminator);
                    }
                    Some(_) => {}
                }
                match bytes.get(index + 2..index + 4) {
                    Some(terminator) if terminator == CRLF => {
                        return Scan::Line(first_cr.unwrap_or(index));
                    }
                    None => return Scan::Incomplete,
                    Some(_) => {}
                }
                first_cr.get_or_insert(index);
                index += 2;
            }
            _ => index += 1,
        }
    }
    Scan::Incomplete
}

fn parse_request_line(line: &[u8]) -> Result<Request, MalformedKind> {
    let mut parts = line.splitn(3, |byte| *byte == b' ');
    let Some(method) = parts.next() else {
        return Err(MalformedKind::MethodToken);
    };
    if !is_method_token(method) {
        return Err(MalformedKind::MethodToken);
    }
    let Some(target) = parts.next() else {
        return Err(MalformedKind::TargetToken);
    };
    if !is_visible_target(target) {
        return Err(MalformedKind::TargetToken);
    }
    let Some(version) = parts.next() else {
        return Err(MalformedKind::VersionToken);
    };
    if version != b"HTTP/1.0" && version != b"HTTP/1.1" {
        return Err(MalformedKind::VersionToken);
    }
    Ok(Request {
        method: classify_method(method),
        target: classify_target(target),
    })
}

const fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_method_token(token: &[u8]) -> bool {
    !token.is_empty() && token.iter().copied().all(is_token_byte)
}

fn is_visible_target(token: &[u8]) -> bool {
    !token.is_empty() && token.iter().all(|byte| matches!(*byte, 0x21..=0x7E))
}

const fn classify_method(token: &[u8]) -> Method {
    match token {
        [b'G', b'E', b'T'] => Method::Get,
        [b'H', b'E', b'A', b'D'] => Method::Head,
        _ => Method::Other,
    }
}

const fn classify_target(token: &[u8]) -> Target {
    match token {
        [b'/'] => Target::Root,
        _ => Target::Other,
    }
}

/// A `HEAD` reply: the header block alone, with the body length the matching
/// `GET` reply declares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Head<'a> {
    head: &'a [u8],
    body_len: usize,
}

impl<'a> Head<'a> {
    /// The header block, ending with the empty line.
    #[must_use]
    pub fn head(self) -> &'a [u8] {
        self.head
    }

    /// The body length declared by `Content-Length`.
    #[must_use]
    pub fn body_len(self) -> usize {
        self.body_len
    }
}

/// Which bytes the connection write path sends for one reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reply<'a> {
    /// A header block and its body in one slice.
    Full(&'a [u8]),
    /// A header block only, for a `HEAD` request.
    HeadOnly(Head<'a>),
}

/// The five reply buffers, rendered once from the build version.
pub struct ResponsePlan {
    ok: Wire,
    not_found: Wire,
    method_not_allowed: Wire,
    bad_request: Wire,
    too_large: Wire,
}

impl ResponsePlan {
    /// Renders the fixed replies for `version`, the text of the liveness line.
    ///
    /// This is the only place that renders a body or a `Content-Length`
    /// value; callers borrow the result for the life of the plan.
    ///
    /// # Panics
    ///
    /// Panics if `version` holds a byte outside visible ASCII or exceeds
    /// [`MAX_VERSION_BYTES`]. The build supplies `CARGO_PKG_VERSION`, so a
    /// panic reports a broken build before any request is served.
    #[must_use]
    pub fn new(version: &str) -> Self {
        assert!(
            version.len() <= MAX_VERSION_BYTES
                && version.bytes().all(|byte| matches!(byte, 0x20..=0x7E)),
            "the build version must fit the liveness body and hold only visible ASCII"
        );
        Self {
            ok: build(
                STATUS_OK,
                &[],
                &[LIVENESS_PREFIX, version.as_bytes(), LIVENESS_SUFFIX],
            ),
            not_found: build(STATUS_NOT_FOUND, &[], &[NOT_FOUND_BODY]),
            method_not_allowed: build(STATUS_METHOD_NOT_ALLOWED, ALLOW, &[METHOD_NOT_ALLOWED_BODY]),
            bad_request: build(STATUS_BAD_REQUEST, &[], &[BAD_REQUEST_BODY]),
            too_large: build(STATUS_TOO_LARGE, &[], &[TOO_LARGE_BODY]),
        }
    }

    /// The complete `200` reply to `GET /`.
    #[must_use]
    pub fn ok_get(&self) -> &[u8] {
        self.ok.full()
    }

    /// The `200` header block for `HEAD /`, declaring the body length the
    /// matching `GET` would have sent.
    #[must_use]
    pub fn ok_head(&self) -> Head<'_> {
        self.ok.head()
    }

    /// The complete `404` reply.
    #[must_use]
    pub fn not_found(&self) -> &[u8] {
        self.not_found.full()
    }

    /// The complete `405` reply, carrying `Allow: GET, HEAD`.
    #[must_use]
    pub fn method_not_allowed(&self) -> &[u8] {
        self.method_not_allowed.full()
    }

    /// The complete `400` reply.
    #[must_use]
    pub fn bad_request(&self) -> &[u8] {
        self.bad_request.full()
    }

    /// The complete `431` reply.
    #[must_use]
    pub fn too_large(&self) -> &[u8] {
        self.too_large.full()
    }

    /// Selects the fixed reply a parsed request earns.
    #[must_use]
    pub fn reply(&self, request: Request) -> Reply<'_> {
        match (request.method(), request.target()) {
            (Method::Other, _) => Reply::Full(self.method_not_allowed.full()),
            (Method::Get, Target::Root) => Reply::Full(self.ok.full()),
            (Method::Head, Target::Root) => Reply::HeadOnly(self.ok.head()),
            (Method::Get, Target::Other) => Reply::Full(self.not_found.full()),
            (Method::Head, Target::Other) => Reply::HeadOnly(self.not_found.head()),
        }
    }
}

/// One planned reply held as a fixed byte array plus its two lengths.
struct Wire {
    bytes: [u8; MAX_REPLY_BYTES],
    len: usize,
    head_len: usize,
}

impl Wire {
    fn full(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn head(&self) -> Head<'_> {
        Head {
            head: &self.bytes[..self.head_len],
            body_len: self.len - self.head_len,
        }
    }
}

/// One ordered segment of a reply header block.
#[derive(Clone, Copy)]
enum Segment<'a> {
    /// Bytes copied to the wire unchanged.
    Fixed(&'a [u8]),
    /// The decimal `Content-Length` value of the body.
    Length,
}

/// The header-block segments of a reply, in wire order.
const fn head_segments<'a>(status: &'a [u8], extra: &'a [u8]) -> [Segment<'a>; 8] {
    [
        Segment::Fixed(status),
        Segment::Fixed(CONTENT_TYPE),
        Segment::Fixed(CONTENT_LENGTH),
        Segment::Length,
        Segment::Fixed(CRLF),
        Segment::Fixed(CONNECTION_CLOSE),
        Segment::Fixed(extra),
        Segment::Fixed(CRLF),
    ]
}

fn build(status: &[u8], extra: &[u8], body: &[&[u8]]) -> Wire {
    let body_len: usize = body.iter().map(|part| part.len()).sum();
    let mut builder = Builder::new();
    for segment in head_segments(status, extra) {
        match segment {
            Segment::Fixed(part) => builder.push(part),
            Segment::Length => builder.push_decimal(body_len),
        }
    }
    let head_len = builder.len();
    for part in body.iter().copied() {
        builder.push(part);
    }
    builder.finish(head_len)
}

/// The worst-case bytes one reply needs, walked over the same segments
/// [`build`] renders.
const fn planned_bytes(status: &[u8], extra: &[u8], body: usize) -> usize {
    let segments = head_segments(status, extra);
    let mut total = 0;
    let mut index = 0;
    while index < segments.len() {
        total += match segments[index] {
            Segment::Fixed(part) => part.len(),
            Segment::Length => MAX_LENGTH_DIGITS,
        };
        index += 1;
    }
    total + body
}

const fn decimal_len(value: usize) -> usize {
    let mut length = 1;
    let mut remaining = value;
    while remaining >= 10 {
        remaining /= 10;
        length += 1;
    }
    length
}

/// An inline reply buffer that grows by copying, never by allocating.
struct Builder {
    bytes: [u8; MAX_REPLY_BYTES],
    len: usize,
}

impl Builder {
    const fn new() -> Self {
        Self {
            bytes: [0; MAX_REPLY_BYTES],
            len: 0,
        }
    }

    fn push(&mut self, part: &[u8]) {
        let end = self.len + part.len();
        assert!(end <= MAX_REPLY_BYTES, "reply exceeds its fixed buffer");
        self.bytes[self.len..end].copy_from_slice(part);
        self.len = end;
    }

    fn push_decimal(&mut self, value: usize) {
        let mut scratch = [0; DECIMAL_DIGITS];
        let length = decimal_len(value);
        let mut remaining = value;
        let mut index = length;
        while index > 0 {
            index -= 1;
            scratch[index] = DECIMAL_TABLE[remaining % 10];
            remaining /= 10;
        }
        self.push(&scratch[..length]);
    }

    const fn len(&self) -> usize {
        self.len
    }

    fn finish(self, head_len: usize) -> Wire {
        Wire {
            bytes: self.bytes,
            len: self.len,
            head_len,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LIVENESS_PREFIX, LIVENESS_SUFFIX, MAX_HEAD_BYTES, MAX_VERSION_BYTES, MalformedKind, Method,
        ParseOutcome, Reply, Request, ResponsePlan, Target, parse_head,
    };

    const OK_HEAD: &[u8] = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Length: 25\r\n",
        "Connection: close\r\n",
        "\r\n",
    )
    .as_bytes();

    const NOT_FOUND_REPLY: &[u8] = concat!(
        "HTTP/1.1 404 Not Found\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Length: 11\r\n",
        "Connection: close\r\n",
        "\r\n",
        "Not found\r\n",
    )
    .as_bytes();

    const METHOD_NOT_ALLOWED_REPLY: &[u8] = concat!(
        "HTTP/1.1 405 Method Not Allowed\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Length: 20\r\n",
        "Connection: close\r\n",
        "Allow: GET, HEAD\r\n",
        "\r\n",
        "Method not allowed\r\n",
    )
    .as_bytes();

    const BAD_REQUEST_REPLY: &[u8] = concat!(
        "HTTP/1.1 400 Bad Request\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Length: 13\r\n",
        "Connection: close\r\n",
        "\r\n",
        "Bad request\r\n",
    )
    .as_bytes();

    const TOO_LARGE_REPLY: &[u8] = concat!(
        "HTTP/1.1 431 Request Header Fields Too Large\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Length: 24\r\n",
        "Connection: close\r\n",
        "\r\n",
        "Request head too large\r\n",
    )
    .as_bytes();

    fn plan() -> ResponsePlan {
        ResponsePlan::new(env!("CARGO_PKG_VERSION"))
    }

    fn liveness_line() -> &'static [u8] {
        concat!("Canary ", env!("CARGO_PKG_VERSION"), " is alive!\r\n").as_bytes()
    }

    fn complete(head: &[u8]) -> Request {
        match parse_head(head) {
            ParseOutcome::Complete(request) => request,
            other => panic!("expected a complete request, got {other:?}"),
        }
    }

    fn reply_bytes(reply: Reply<'_>) -> &[u8] {
        match reply {
            Reply::Full(bytes) => bytes,
            Reply::HeadOnly(head) => head.head(),
        }
    }

    fn body_of(reply: &[u8]) -> &[u8] {
        let split = reply
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        &reply[split..]
    }

    fn declared_body_len(reply: &[u8]) -> usize {
        let text = std::str::from_utf8(reply).unwrap();
        let line = text
            .lines()
            .find(|line| line.starts_with("Content-Length: "))
            .unwrap();
        line["Content-Length: ".len()..].parse().unwrap()
    }

    fn head_padded_to(length: usize) -> Vec<u8> {
        let mut head = Vec::from(&b"GET / HTTP/1.1\r\nX: "[..]);
        head.resize(length - 4, b'a');
        head.extend_from_slice(b"\r\n\r\n");
        head
    }

    #[test]
    fn max_head_is_one_kibibyte() {
        assert_eq!(MAX_HEAD_BYTES, 1024);
    }

    #[test]
    fn complete_root_get() {
        let request = complete(b"GET / HTTP/1.1\r\n\r\n");
        assert_eq!(request.method(), Method::Get);
        assert_eq!(request.target(), Target::Root);
    }

    #[test]
    fn complete_root_head() {
        let request = complete(b"HEAD / HTTP/1.1\r\n\r\n");
        assert_eq!(request.method(), Method::Head);
        assert_eq!(request.target(), Target::Root);
    }

    #[test]
    fn version_one_zero_is_accepted() {
        let request = complete(b"GET / HTTP/1.0\r\n\r\n");
        assert_eq!(request.method(), Method::Get);
    }

    #[test]
    fn lower_case_method_is_other() {
        assert_eq!(complete(b"get / HTTP/1.1\r\n\r\n").method(), Method::Other);
    }

    #[test]
    fn query_target_is_other() {
        assert_eq!(
            complete(b"GET /?x HTTP/1.1\r\n\r\n").target(),
            Target::Other
        );
    }

    #[test]
    fn absolute_form_target_is_other() {
        let request = complete(b"GET http://example.com/ HTTP/1.1\r\n\r\n");
        assert_eq!(request.target(), Target::Other);
    }

    #[test]
    fn traversal_target_is_other() {
        let request = complete(b"GET /../etc/passwd HTTP/1.1\r\n\r\n");
        assert_eq!(request.target(), Target::Other);
    }

    #[test]
    fn header_lines_after_request_line_are_ignored() {
        let request = complete(b"GET / HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\n\r\n");
        assert_eq!(request.method(), Method::Get);
        assert_eq!(request.target(), Target::Root);
    }

    #[test]
    fn partial_head_is_incomplete() {
        let head = head_padded_to(MAX_HEAD_BYTES);
        for end in 0..head.len() {
            assert_eq!(
                parse_head(&head[..end]),
                ParseOutcome::Incomplete,
                "prefix of length {end}"
            );
        }
    }

    #[test]
    fn head_at_the_limit_is_accepted() {
        let head = head_padded_to(MAX_HEAD_BYTES);
        assert_eq!(head.len(), MAX_HEAD_BYTES);
        assert!(matches!(parse_head(&head), ParseOutcome::Complete(_)));
    }

    #[test]
    fn head_one_byte_over_the_limit_is_too_large() {
        let head = head_padded_to(MAX_HEAD_BYTES + 1);
        assert_eq!(head.len(), 1025);
        assert_eq!(parse_head(&head), ParseOutcome::TooLarge);
    }

    #[test]
    fn head_with_a_suffix_after_the_terminator_is_complete() {
        let mut head = Vec::from(&b"GET / HTTP/1.1\r\n\r\n"[..]);
        head.resize(head.len() + 1040, b'x');
        assert!(
            head.len() > MAX_HEAD_BYTES,
            "the offer exceeds the window: {} bytes",
            head.len()
        );
        assert!(matches!(parse_head(&head), ParseOutcome::Complete(_)));
    }

    #[test]
    fn full_window_without_a_terminator_is_too_large() {
        let mut head = Vec::from(&b"GET / HTTP/1.1\r\nX: "[..]);
        head.resize(MAX_HEAD_BYTES, b'a');
        assert!(!head.windows(4).any(|window| window == b"\r\n\r\n"));
        assert_eq!(parse_head(&head), ParseOutcome::TooLarge);
    }

    #[test]
    fn malformed_line_in_the_window_wins_over_too_large() {
        let mut head = Vec::from(&b"GET / HTTP/1.1\n"[..]);
        head.resize(MAX_HEAD_BYTES + 16, b'a');
        assert_eq!(
            parse_head(&head),
            ParseOutcome::Malformed(MalformedKind::MissingTerminator)
        );
    }

    #[test]
    fn bare_line_feed_is_malformed() {
        assert_eq!(
            parse_head(b"GET / HTTP/1.1\n\n"),
            ParseOutcome::Malformed(MalformedKind::MissingTerminator)
        );
    }

    #[test]
    fn missing_target_is_malformed() {
        assert_eq!(
            parse_head(b"GET  HTTP/1.1\r\n\r\n"),
            ParseOutcome::Malformed(MalformedKind::TargetToken)
        );
    }

    #[test]
    fn missing_version_is_malformed() {
        assert_eq!(
            parse_head(b"GET /\r\n\r\n"),
            ParseOutcome::Malformed(MalformedKind::VersionToken)
        );
    }

    #[test]
    fn unknown_version_is_malformed() {
        assert_eq!(
            parse_head(b"GET / HTTP/2.0\r\n\r\n"),
            ParseOutcome::Malformed(MalformedKind::VersionToken)
        );
    }

    #[test]
    fn empty_request_line_is_malformed() {
        assert_eq!(
            parse_head(b"\r\n\r\n"),
            ParseOutcome::Malformed(MalformedKind::MethodToken)
        );
    }

    #[test]
    fn tab_in_method_is_malformed() {
        assert_eq!(
            parse_head(b"GET\t/ HTTP/1.1\r\n\r\n"),
            ParseOutcome::Malformed(MalformedKind::MethodToken)
        );
    }

    #[test]
    fn ok_get_is_the_head_and_the_liveness_line() {
        let plan = plan();
        let expected = [OK_HEAD, liveness_line()].concat();
        assert_eq!(plan.ok_get(), expected.as_slice());
    }

    #[test]
    fn ok_body_is_exactly_the_liveness_line() {
        let plan = plan();
        let body = body_of(plan.ok_get());
        assert_eq!(body, liveness_line());
        assert_eq!(body.len(), 25);
    }

    #[test]
    fn ok_head_carries_the_head_only() {
        let plan = plan();
        let head = plan.ok_head();
        assert_eq!(head.head(), OK_HEAD);
        assert_eq!(head.body_len(), 25);
    }

    #[test]
    fn not_found_is_byte_exact() {
        let plan = plan();
        assert_eq!(plan.not_found(), NOT_FOUND_REPLY);
    }

    #[test]
    fn method_not_allowed_is_byte_exact() {
        let plan = plan();
        assert_eq!(plan.method_not_allowed(), METHOD_NOT_ALLOWED_REPLY);
    }

    #[test]
    fn bad_request_is_byte_exact() {
        let plan = plan();
        assert_eq!(plan.bad_request(), BAD_REQUEST_REPLY);
    }

    #[test]
    fn too_large_is_byte_exact() {
        let plan = plan();
        assert_eq!(plan.too_large(), TOO_LARGE_REPLY);
    }

    #[test]
    fn every_declared_length_matches_its_body() {
        let plan = plan();
        for reply in [
            plan.ok_get(),
            plan.not_found(),
            plan.method_not_allowed(),
            plan.bad_request(),
            plan.too_large(),
        ] {
            assert_eq!(declared_body_len(reply), body_of(reply).len());
        }
        assert_eq!(plan.ok_head().body_len(), 25);
    }

    #[test]
    fn root_get_selects_the_ok_reply() {
        let plan = plan();
        let reply = plan.reply(complete(b"GET / HTTP/1.1\r\n\r\n"));
        assert_eq!(reply_bytes(reply), plan.ok_get());
    }

    #[test]
    fn root_head_selects_the_head_only_reply() {
        let plan = plan();
        let reply = plan.reply(complete(b"HEAD / HTTP/1.1\r\n\r\n"));
        assert_eq!(reply, Reply::HeadOnly(plan.ok_head()));
        assert_eq!(reply_bytes(reply), OK_HEAD);
    }

    #[test]
    fn other_method_selects_the_405_reply() {
        let plan = plan();
        let reply = plan.reply(complete(b"get / HTTP/1.1\r\n\r\n"));
        assert_eq!(reply_bytes(reply), plan.method_not_allowed());
    }

    #[test]
    fn other_target_selects_the_404_reply() {
        let plan = plan();
        let reply = plan.reply(complete(b"GET /?x HTTP/1.1\r\n\r\n"));
        assert_eq!(reply_bytes(reply), plan.not_found());
    }

    #[test]
    fn other_target_head_selects_the_head_only_404_reply() {
        let plan = plan();
        let head_len = NOT_FOUND_REPLY.len() - 11;
        let reply = plan.reply(complete(b"HEAD /?x HTTP/1.1\r\n\r\n"));
        let Reply::HeadOnly(head) = reply else {
            panic!("expected a head-only reply, got {reply:?}");
        };
        assert_eq!(head.head(), &NOT_FOUND_REPLY[..head_len]);
        assert_eq!(head.body_len(), 11);
    }

    #[test]
    fn malformed_request_line_is_malformed() {
        assert_eq!(
            parse_head(b"GET / HTTP/1.1\rHost: x\r\n\r\n"),
            ParseOutcome::Malformed(MalformedKind::MissingTerminator)
        );
    }

    #[test]
    fn version_at_the_limit_is_accepted() {
        let version = "1".repeat(MAX_VERSION_BYTES);
        assert_eq!(version.len(), MAX_VERSION_BYTES);
        let plan = ResponsePlan::new(&version);
        let reply = plan.ok_get();
        assert_eq!(declared_body_len(reply), body_of(reply).len());
        assert_eq!(
            body_of(reply).len(),
            LIVENESS_PREFIX.len() + MAX_VERSION_BYTES + LIVENESS_SUFFIX.len()
        );
    }

    #[test]
    #[should_panic(expected = "version")]
    fn version_over_the_limit_panics() {
        let _plan = ResponsePlan::new(&"1".repeat(MAX_VERSION_BYTES + 1));
    }

    #[test]
    #[should_panic(expected = "visible ASCII")]
    fn version_with_a_control_byte_panics() {
        let _plan = ResponsePlan::new("1.0\t");
    }
}
