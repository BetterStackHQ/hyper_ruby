// RFC 6587 framing: octet counting (`<len> <message>`) with a non-transparent
// newline fallback. A buffer whose first byte is a non-zero digit is treated as
// octet counted, everything else is newline delimited.
//
// This file (including its test data) is a port of Vector 0.48.0's
// lib/codecs/src/decoding/framing/octet_counting.rs and is licensed under the
// Mozilla Public License 2.0, not the MIT licence covering the rest of this
// gem. See https://github.com/vectordotdev/vector and
// https://www.mozilla.org/en-US/MPL/2.0/. Deliberate divergences from that
// source are marked below.

use bytes::{Buf, Bytes, BytesMut};
use tokio_util::codec::{Decoder, LinesCodec, LinesCodecError};

/// Why a frame was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RejectReason {
    Oversize,
    InvalidUtf8,
    InvalidLength,
}

impl RejectReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RejectReason::Oversize => "oversize",
            RejectReason::InvalidUtf8 => "invalid_utf8",
            RejectReason::InvalidLength => "invalid_length",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameError {
    pub reason: RejectReason,
    /// A fatal error leaves the stream out of sync: the frame boundary can no
    /// longer be established, so the connection must close. A non-fatal error
    /// skips the offending frame only.
    pub fatal: bool,
}

impl FrameError {
    fn fatal(reason: RejectReason) -> Self {
        Self { reason, fatal: true }
    }

    fn recoverable(reason: RejectReason) -> Self {
        Self { reason, fatal: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    NotDiscarding,
    Discarding(usize),
    DiscardingToEol,
}

pub(crate) struct Framer {
    other: LinesCodec,
    octet_decoding: Option<State>,
}

impl Framer {
    pub(crate) fn new(max_length: usize) -> Self {
        Self {
            other: LinesCodec::new_with_max_length(max_length),
            octet_decoding: None,
        }
    }

    /// Decode the next complete frame, if the buffer holds one.
    pub(crate) fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Bytes>, FrameError> {
        match self.checked_decode(src) {
            Some(result) => result,
            None => self
                .other
                .decode(src)
                .map(|line| line.map(Bytes::from))
                .map_err(line_error),
        }
    }

    /// Decode at end of stream, which releases a trailing unterminated line.
    pub(crate) fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Bytes>, FrameError> {
        match self.checked_decode(src) {
            Some(result) => result,
            None => self
                .other
                .decode_eof(src)
                .map(|line| line.map(Bytes::from))
                .map_err(line_error),
        }
    }

    /// `None` if this buffer is not octet counting encoded.
    fn checked_decode(&mut self, src: &mut BytesMut) -> Option<Result<Option<Bytes>, FrameError>> {
        // Divergence from the ported source: only a decoder with no state in
        // hand starts a new octet counted frame. The source re-enters
        // `NotDiscarding` whenever the buffer happens to start with a digit,
        // so a read boundary that lands on a digit inside an oversize body
        // loses the countdown and hands the rest of that body over as
        // messages.
        if self.octet_decoding.is_none() {
            if let Some(&first_byte) = src.first() {
                if (b'1'..=b'9').contains(&first_byte) {
                    self.octet_decoding = Some(State::NotDiscarding);
                }
            }
        }

        self.octet_decoding
            .map(|state| self.octet_decode(state, src))
    }

    fn octet_decode(
        &mut self,
        state: State,
        src: &mut BytesMut,
    ) -> Result<Option<Bytes>, FrameError> {
        // Encoding scheme: an ASCII decimal length, a space, then that many
        // bytes of message.
        let space_pos = src.iter().position(|&b| b == b' ');
        let newline_pos = src.iter().position(|&b| b == b'\n');

        match (state, newline_pos, space_pos) {
            (State::Discarding(chars), _, _) if src.len() >= chars => {
                // Enough bytes buffered to finish discarding the oversize frame.
                src.advance(chars);
                self.octet_decoding = None;
                Err(FrameError::fatal(RejectReason::Oversize))
            }

            (State::Discarding(chars), _, _) => {
                // Not enough bytes yet; discard what we have and carry the
                // remainder forward. Divergence from the ported source, which
                // subtracts these the other way around: with overflow checks
                // off, as in its release builds, the count wraps to a huge
                // number and the frame is never reported, so this countdown is
                // stricter than that source's behaviour.
                self.octet_decoding = Some(State::Discarding(chars - src.len()));
                src.advance(src.len());
                Ok(None)
            }

            (State::DiscardingToEol, Some(offset), _) => {
                src.advance(offset + 1);
                self.octet_decoding = None;
                Err(FrameError::fatal(RejectReason::Oversize))
            }

            (State::DiscardingToEol, None, _) => {
                // No newline to sync on yet; discard the whole buffer.
                src.advance(src.len());
                Ok(None)
            }

            (State::NotDiscarding, _, Some(space_pos)) if space_pos < self.other.max_length() => {
                let len: usize = match std::str::from_utf8(&src[..space_pos])
                    .map_err(|_| ())
                    .and_then(|num| num.parse().map_err(|_| ()))
                {
                    Ok(len) => len,
                    Err(_) => {
                        // Not a sensible number; step past it so we cannot loop
                        // on the same bytes forever.
                        src.advance(space_pos + 1);
                        self.octet_decoding = None;
                        return Err(FrameError::fatal(RejectReason::InvalidLength));
                    }
                };

                let from = space_pos + 1;
                let to = from + len;

                if len > self.other.max_length() {
                    // Discard the declared length before reporting the error, so
                    // the message body cannot be mistaken for further frames.
                    self.octet_decoding = Some(State::Discarding(len));
                    src.advance(space_pos + 1);
                    Ok(None)
                } else if let Some(msg) = src.get(from..to) {
                    let bytes = match std::str::from_utf8(msg) {
                        Ok(_) => Bytes::copy_from_slice(msg),
                        Err(_) => {
                            src.advance(to);
                            self.octet_decoding = None;
                            return Err(FrameError::fatal(RejectReason::InvalidUtf8));
                        }
                    };

                    src.advance(to);
                    self.octet_decoding = None;
                    Ok(Some(bytes))
                } else {
                    // Acceptable length, but the message is not all here yet.
                    Ok(None)
                }
            }

            (State::NotDiscarding, Some(newline_pos), _) => {
                // Beyond the maximum length; advance to the newline.
                src.advance(newline_pos + 1);
                Err(FrameError::fatal(RejectReason::Oversize))
            }

            (State::NotDiscarding, None, _) if src.len() < self.other.max_length() => Ok(None),

            (State::NotDiscarding, None, _) => {
                // More data than we will handle and nothing to sync on.
                self.octet_decoding = Some(State::DiscardingToEol);
                src.advance(src.len());
                Ok(None)
            }
        }
    }
}

fn line_error(error: LinesCodecError) -> FrameError {
    match error {
        LinesCodecError::MaxLineLengthExceeded => FrameError::recoverable(RejectReason::Oversize),
        // The line codec only fails with an IO error for non-UTF-8 input.
        LinesCodecError::Io(_) => FrameError::fatal(RejectReason::InvalidUtf8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;

    fn decode_all(framer: &mut Framer, buffer: &mut BytesMut) -> Vec<Result<Bytes, FrameError>> {
        let mut out = Vec::new();
        loop {
            match framer.decode(buffer) {
                Ok(Some(frame)) => out.push(Ok(frame)),
                Ok(None) => return out,
                Err(error) => {
                    out.push(Err(error));
                    if error.fatal {
                        return out;
                    }
                }
            }
        }
    }

    #[test]
    fn non_octet_decode_works_with_multiple_frames() {
        let mut decoder = Framer::new(128);
        let mut buffer = BytesMut::with_capacity(16);

        buffer.put(&b"<57>Mar 25 21:47:46 host quaerat[2444]: There were "[..]);
        assert_eq!(Ok(None), decoder.decode(&mut buffer));

        buffer.put(&b"8 penguins in the shop.\n"[..]);
        assert_eq!(
            Ok(Some(Bytes::from(
                "<57>Mar 25 21:47:46 host quaerat[2444]: There were 8 penguins in the shop."
            ))),
            decoder.decode(&mut buffer)
        );
    }

    #[test]
    fn newline_decode_strips_carriage_return() {
        let mut decoder = Framer::new(128);
        let mut buffer = BytesMut::from(&b"<13>first\r\n<13>second\n"[..]);

        assert_eq!(Ok(Some(Bytes::from("<13>first"))), decoder.decode(&mut buffer));
        assert_eq!(Ok(Some(Bytes::from("<13>second"))), decoder.decode(&mut buffer));
        assert_eq!(Ok(None), decoder.decode(&mut buffer));
    }

    #[test]
    fn octet_decode_works_with_multiple_frames() {
        let mut decoder = Framer::new(30);
        let mut buffer = BytesMut::with_capacity(16);

        buffer.put(&b"28 abcdefghijklm"[..]);
        assert_eq!(Ok(None), decoder.decode(&mut buffer));

        // A frame starting with a number mid-message must not start a new message.
        buffer.put(&b"3 nopqrstuvwxyz"[..]);
        assert_eq!(
            Ok(Some(Bytes::from("abcdefghijklm3 nopqrstuvwxyz"))),
            decoder.decode(&mut buffer)
        );
    }

    #[test]
    fn octet_decode_moves_past_invalid_length() {
        let mut decoder = Framer::new(16);
        let mut buffer = BytesMut::from(&b"232>1 zork"[..]);

        assert_eq!(
            Err(FrameError::fatal(RejectReason::InvalidLength)),
            decoder.decode(&mut buffer)
        );
        assert_eq!(b"zork"[..], buffer);
    }

    #[test]
    fn octet_decode_moves_past_invalid_utf8() {
        let mut decoder = Framer::new(16);
        let mut buffer = BytesMut::from(&[b'4', b' ', 0xf0, 0x28, 0x8c, 0xbc][..]);

        assert_eq!(
            Err(FrameError::fatal(RejectReason::InvalidUtf8)),
            decoder.decode(&mut buffer)
        );
        assert_eq!(b""[..], buffer);
    }

    #[test]
    fn octet_decode_moves_past_exceeded_frame_length() {
        let mut decoder = Framer::new(16);
        let mut buffer = BytesMut::from(
            &b"32thisshouldbelongerthanthmaxframeasizewhichmeansitwillnotbedecoded\n"[..],
        );

        assert_eq!(
            Err(FrameError::fatal(RejectReason::Oversize)),
            decoder.decode(&mut buffer)
        );
        assert_eq!(b""[..], buffer);
    }

    #[test]
    fn octet_decode_rejects_exceeded_frame_length() {
        let mut decoder = Framer::new(16);
        let mut buffer = BytesMut::from(&b"26 abcdefghijklmnopqrstuvwxyzand here we are"[..]);

        assert_eq!(Ok(None), decoder.decode(&mut buffer));
        assert_eq!(
            Err(FrameError::fatal(RejectReason::Oversize)),
            decoder.decode(&mut buffer)
        );
        assert_eq!(b"and here we are"[..], buffer);
    }

    #[test]
    fn octet_decode_rejects_exceeded_frame_length_multiple_frames() {
        let mut decoder = Framer::new(16);
        let mut buffer = BytesMut::from(&b"26 abc"[..]);
        let _ = decoder.decode(&mut buffer);

        buffer.put(&b"defghijklmnopqrstuvwxyzand here we are"[..]);
        assert_eq!(
            Err(FrameError::fatal(RejectReason::Oversize)),
            decoder.decode(&mut buffer)
        );
        assert_eq!(b"and here we are"[..], buffer);
    }

    #[test]
    fn octet_decode_discards_oversize_body_arriving_in_pieces() {
        let mut decoder = Framer::new(16);
        let mut buffer = BytesMut::from(&b"26 abc"[..]);
        assert_eq!(Ok(None), decoder.decode(&mut buffer));

        for _ in 0..10 {
            buffer.put(&b"ab"[..]);
            assert_eq!(Ok(None), decoder.decode(&mut buffer));
        }

        buffer.put(&b"xyzrest"[..]);
        assert_eq!(
            Err(FrameError::fatal(RejectReason::Oversize)),
            decoder.decode(&mut buffer)
        );
        assert_eq!(b"rest"[..], buffer);
    }

    #[test]
    fn octet_decode_moves_past_exceeded_frame_length_multiple_frames() {
        let mut decoder = Framer::new(16);
        let mut buffer = BytesMut::from(
            &b"32thisshouldbelongerthanthmaxframeasizewhichmeansitwillnotbedecoded"[..],
        );
        let _ = decoder.decode(&mut buffer);

        buffer.put(&b"wemustcontinuetodiscard\n32 something valid"[..]);
        assert_eq!(
            Err(FrameError::fatal(RejectReason::Oversize)),
            decoder.decode(&mut buffer)
        );
        assert_eq!(b"32 something valid"[..], buffer);
    }

    #[test]
    fn oversize_body_starting_with_a_digit_is_not_decoded_as_a_frame() {
        let mut decoder = Framer::new(16);
        let mut buffer = BytesMut::from(&b"26 "[..]);
        assert_eq!(Ok(None), decoder.decode(&mut buffer));

        // A read boundary leaving a digit at the front of the discarded body.
        buffer.put(&b"9 abcdefghZ"[..]);
        assert_eq!(Ok(None), decoder.decode(&mut buffer));
        assert_eq!(b""[..], buffer);

        buffer.put(&b"aaaaaaaaaaaaaaa5 next"[..]);
        assert_eq!(
            Err(FrameError::fatal(RejectReason::Oversize)),
            decoder.decode(&mut buffer)
        );
        assert_eq!(b"5 next"[..], buffer);
    }

    #[test]
    fn newline_oversize_is_recoverable() {
        let mut decoder = Framer::new(16);
        let mut buffer = BytesMut::from(&b"<13>aaaaaaaaaaaaaaaaaaaaaaaaaaaa\n<13>ok\n"[..]);

        let results = decode_all(&mut decoder, &mut buffer);
        assert_eq!(
            vec![
                Err(FrameError::recoverable(RejectReason::Oversize)),
                Ok(Bytes::from("<13>ok")),
            ],
            results
        );
    }

    #[test]
    fn newline_invalid_utf8_is_fatal() {
        let mut decoder = Framer::new(64);
        let mut buffer = BytesMut::from(&[b'<', b'1', b'3', b'>', 0xf0, 0x28, b'\n'][..]);

        assert_eq!(
            Err(FrameError::fatal(RejectReason::InvalidUtf8)),
            decoder.decode(&mut buffer)
        );
    }

    #[test]
    fn decode_eof_releases_trailing_line() {
        let mut decoder = Framer::new(64);
        let mut buffer = BytesMut::from(&b"<13>trailing"[..]);

        assert_eq!(Ok(None), decoder.decode(&mut buffer));
        assert_eq!(
            Ok(Some(Bytes::from("<13>trailing"))),
            decoder.decode_eof(&mut buffer)
        );
        assert_eq!(Ok(None), decoder.decode_eof(&mut buffer));
    }

    #[test]
    fn decode_eof_holds_back_incomplete_octet_frame() {
        let mut decoder = Framer::new(64);
        let mut buffer = BytesMut::from(&b"10 partial"[..]);

        assert_eq!(Ok(None), decoder.decode(&mut buffer));
        assert_eq!(Ok(None), decoder.decode_eof(&mut buffer));
    }

    #[test]
    fn frames_decode_identically_at_every_split_point() {
        let inputs: Vec<&[u8]> = vec![
            b"11 hello there7 goodbye<13>newline\n<13>another\r\n",
            b"5 first<13>second\n6 third\n",
            b"<13>only a line\n",
            b"13 embedded\nline<13>after\n",
        ];

        for input in inputs {
            let mut whole = BytesMut::from(input);
            let expected = decode_all(&mut Framer::new(64), &mut whole);

            for split in 1..input.len() {
                let mut framer = Framer::new(64);
                let mut buffer = BytesMut::new();
                let mut actual = Vec::new();

                buffer.put(&input[..split]);
                actual.extend(decode_all(&mut framer, &mut buffer));
                buffer.put(&input[split..]);
                actual.extend(decode_all(&mut framer, &mut buffer));

                assert_eq!(
                    expected,
                    actual,
                    "split {} of {:?}",
                    split,
                    String::from_utf8_lossy(input)
                );
            }
        }
    }

    #[test]
    fn frames_decode_identically_byte_at_a_time() {
        let input: &[u8] = b"11 hello there<13>newline\n7 goodbye";
        let mut whole = BytesMut::from(input);
        let expected = decode_all(&mut Framer::new(64), &mut whole);

        let mut framer = Framer::new(64);
        let mut buffer = BytesMut::new();
        let mut actual = Vec::new();
        for byte in input {
            buffer.put_u8(*byte);
            actual.extend(decode_all(&mut framer, &mut buffer));
        }

        assert_eq!(expected, actual);
    }
}
