// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{cmp::min, fmt::Debug};

use neqo_common::{
    hex_with_len, qtrace, Decoder, IncrementalDecoderBuffer, IncrementalDecoderIgnore,
    IncrementalDecoderUint,
};
use neqo_transport::{Connection, StreamId};

use super::hframe::HFrameType;
use crate::{Error, RecvStream, Res};

const MAX_READ_SIZE: usize = 2048; // Given a practical MTU of 1500 bytes, this seems reasonable.

pub trait FrameDecoder<T> {
    fn is_known_type(frame_type: HFrameType) -> bool;

    /// # Errors
    ///
    /// Returns `HttpFrameUnexpected` if frames is not allowed, i.e. is a `H3_RESERVED_FRAME_TYPES`.
    fn frame_type_allowed(_frame_type: HFrameType) -> Res<()> {
        Ok(())
    }

    /// # Errors
    ///
    /// If a frame cannot be properly decoded.
    fn decode(frame_type: HFrameType, frame_len: u64, data: Option<&[u8]>) -> Res<Option<T>>;
}

#[expect(clippy::module_name_repetitions, reason = "This is OK.")]
pub trait StreamReader {
    /// # Errors
    ///
    /// An error may happen while reading a stream, e.g. early close, protocol error, etc.
    /// Return an error if the stream was closed on the transport layer, but that information is not
    /// yet consumed on the  http/3 layer.
    fn read_data(&mut self, buf: &mut [u8]) -> Res<(usize, bool)>;
}

pub struct StreamReaderConnectionWrapper<'a> {
    conn: &'a mut Connection,
    stream_id: StreamId,
}

impl<'a> StreamReaderConnectionWrapper<'a> {
    pub fn new(conn: &'a mut Connection, stream_id: StreamId) -> Self {
        Self { conn, stream_id }
    }
}

impl StreamReader for StreamReaderConnectionWrapper<'_> {
    /// # Errors
    ///
    /// An error may happen while reading a stream, e.g. early close, protocol error, etc.
    fn read_data(&mut self, buf: &mut [u8]) -> Res<(usize, bool)> {
        let res = self.conn.stream_recv(self.stream_id, buf)?;
        Ok(res)
    }
}

pub struct StreamReaderRecvStreamWrapper<'a> {
    recv_stream: &'a mut Box<dyn RecvStream>,
    conn: &'a mut Connection,
}

impl<'a> StreamReaderRecvStreamWrapper<'a> {
    pub fn new(conn: &'a mut Connection, recv_stream: &'a mut Box<dyn RecvStream>) -> Self {
        Self { recv_stream, conn }
    }
}

impl StreamReader for StreamReaderRecvStreamWrapper<'_> {
    /// # Errors
    ///
    /// An error may happen while reading a stream, e.g. early close, protocol error, etc.
    fn read_data(&mut self, buf: &mut [u8]) -> Res<(usize, bool)> {
        self.recv_stream.read_data(self.conn, buf)
    }
}

#[derive(Clone, Debug)]
enum FrameReaderState {
    GetType { decoder: IncrementalDecoderUint },
    GetLength { decoder: IncrementalDecoderUint },
    GetData { decoder: IncrementalDecoderBuffer },
    UnknownFrameDischargeData { decoder: IncrementalDecoderIgnore },
}

#[derive(Debug)]
#[expect(clippy::module_name_repetitions, reason = "This is OK.")]
pub struct FrameReader {
    state: FrameReaderState,
    frame_type: HFrameType,
    frame_len: u64,
    buffer: [u8; MAX_READ_SIZE],
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: FrameReaderState::GetType {
                decoder: IncrementalDecoderUint::default(),
            },
            frame_type: HFrameType(u64::MAX),
            frame_len: 0,
            buffer: [0; MAX_READ_SIZE],
        }
    }

    #[must_use]
    pub fn new_with_type(frame_type: HFrameType) -> Self {
        Self {
            state: FrameReaderState::GetLength {
                decoder: IncrementalDecoderUint::default(),
            },
            frame_type,
            frame_len: 0,
            buffer: [0; MAX_READ_SIZE],
        }
    }

    fn reset(&mut self) {
        self.state = FrameReaderState::GetType {
            decoder: IncrementalDecoderUint::default(),
        };
    }

    fn min_remaining(&self) -> usize {
        match &self.state {
            FrameReaderState::GetType { decoder } | FrameReaderState::GetLength { decoder } => {
                decoder.min_remaining()
            }
            FrameReaderState::GetData { decoder } => decoder.min_remaining(),
            FrameReaderState::UnknownFrameDischargeData { decoder } => decoder.min_remaining(),
        }
    }

    const fn decoding_in_progress(&self) -> bool {
        if let FrameReaderState::GetType { decoder } = &self.state {
            decoder.decoding_in_progress()
        } else {
            true
        }
    }

    /// Returns true if QUIC stream was closed.
    ///
    /// # Errors
    ///
    /// May return `HttpFrame` if a frame cannot be decoded.
    /// and `TransportStreamDoesNotExist` if `stream_recv` fails.
    pub fn receive<T: FrameDecoder<T>>(
        &mut self,
        stream_reader: &mut dyn StreamReader,
    ) -> Res<(Option<T>, bool)> {
        loop {
            let to_read = min(self.min_remaining(), self.buffer.len());
            let (output, read, fin) = match stream_reader
                .read_data(&mut self.buffer[..to_read])
                .map_err(|e| Error::map_stream_recv_errors(&e))?
            {
                (0, f) => (None, false, f),
                (amount, f) => {
                    qtrace!("FrameReader::receive: reading {amount} byte, fin={f}");
                    (self.consume::<T>(amount)?, true, f)
                }
            };

            if output.is_some() {
                break Ok((output, fin));
            }

            if fin {
                if self.decoding_in_progress() {
                    break Err(Error::HttpFrame);
                }
                break Ok((None, fin));
            }

            if !read {
                // There was no new data, exit the loop.
                break Ok((None, false));
            }
        }
    }

    /// # Errors
    ///
    /// May return `HttpFrame` if a frame cannot be decoded.
    fn consume<T: FrameDecoder<T>>(&mut self, amount: usize) -> Res<Option<T>> {
        let mut input = Decoder::from(&self.buffer[..amount]);
        match &mut self.state {
            FrameReaderState::GetType { decoder } => {
                if let Some(v) = decoder.consume(&mut input) {
                    qtrace!("FrameReader::receive: read frame type {v}");
                    self.frame_type_decoded::<T>(HFrameType(v))?;
                }
            }
            FrameReaderState::GetLength { decoder } => {
                if let Some(len) = decoder.consume(&mut input) {
                    qtrace!(
                        "FrameReader::receive: frame type {:?} length {len}",
                        self.frame_type
                    );
                    return self.frame_length_decoded::<T>(len);
                }
            }
            FrameReaderState::GetData { decoder } => {
                if let Some(data) = decoder.consume(&mut input) {
                    qtrace!(
                        "received frame {:?}: {}",
                        self.frame_type,
                        hex_with_len(&data[..])
                    );
                    return self.frame_data_decoded::<T>(&data);
                }
            }
            FrameReaderState::UnknownFrameDischargeData { decoder } => {
                if decoder.consume(&mut input) {
                    self.reset();
                }
            }
        }
        Ok(None)
    }
    fn frame_type_decoded<T: FrameDecoder<T>>(&mut self, frame_type: HFrameType) -> Res<()> {
        T::frame_type_allowed(frame_type)?;
        self.frame_type = frame_type;
        self.state = FrameReaderState::GetLength {
            decoder: IncrementalDecoderUint::default(),
        };
        Ok(())
    }

    fn frame_length_decoded<T: FrameDecoder<T>>(&mut self, len: u64) -> Res<Option<T>> {
        self.frame_len = len;
        if let Some(f) = T::decode(
            self.frame_type,
            self.frame_len,
            if len > 0 { None } else { Some(&[]) },
        )? {
            self.reset();
            return Ok(Some(f));
        } else if T::is_known_type(self.frame_type) {
            self.state = FrameReaderState::GetData {
                decoder: IncrementalDecoderBuffer::new(
                    usize::try_from(len).or(Err(Error::HttpFrame))?,
                ),
            };
        } else if self.frame_len == 0 {
            self.reset();
        } else {
            self.state = FrameReaderState::UnknownFrameDischargeData {
                decoder: IncrementalDecoderIgnore::new(
                    usize::try_from(len).or(Err(Error::HttpFrame))?,
                ),
            };
        }
        Ok(None)
    }

    fn frame_data_decoded<T: FrameDecoder<T>>(&mut self, data: &[u8]) -> Res<Option<T>> {
        let res = T::decode(self.frame_type, self.frame_len, Some(data))?;
        self.reset();
        Ok(res)
    }
}
