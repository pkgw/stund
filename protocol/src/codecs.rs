// Copyright 2018 Peter Williams <peter@newton.cx>
// Licensed under the MIT License.

//! Codec helpers for the communication protocol.
//!
//! This module is a bit of hack. Basically, we end up needed some shim code
//! to get all of our dependencies to play nicely together with regards to
//! errors.

use bytes::BytesMut;
use serde::{Deserialize, Serialize};
use tokio_codec::{BytesCodec, Decoder, Encoder, FramedRead, FramedWrite};
use tokio_io::io::{ReadHalf, WriteHalf};
use tokio_io::AsyncRead;
use tokio_serde_bincode::{ReadBincode, WriteBincode};
use tokio_uds::UnixStream;

use super::*;

/// A shim type for Tokio codecs.
///
/// This type is just like `tokio_codec::BytesCodec`, but its error type is
/// `failure::Error`.
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FailureBytesCodec(BytesCodec);

impl FailureBytesCodec {
    /// Obtain a new `FailureBytesCodec`.
    pub fn new() -> FailureBytesCodec {
        FailureBytesCodec(BytesCodec::new())
    }
}

impl Decoder for FailureBytesCodec {
    type Item = <BytesCodec as Decoder>::Item;
    type Error = failure::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        Ok(self.0.decode(src)?)
    }
}

impl Encoder for FailureBytesCodec {
    type Item = <BytesCodec as Encoder>::Item;
    type Error = failure::Error;

    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        Ok(self.0.encode(item, dst)?)
    }
}

/// A serializer for a stund message.
///
/// This serializes using "bincode" encoding over a Unix socket stream, using our
/// special `failure`-compatible bytes codec.
pub type Serializer<M> = WriteBincode<FramedWrite<WriteHalf<UnixStream>, FailureBytesCodec>, M>;

/// A deserializer for a stund message.
///
/// This deserializes using "bincode" encoding over a Unix socket stream, using our
/// special `failure`-compatible bytes codec.
pub type Deserializer<M> = ReadBincode<FramedRead<ReadHalf<UnixStream>, FailureBytesCodec>, M>;

/// Split a Unix socket stream into a stund serializer and deserializer.
pub fn split<S, D>(conn: UnixStream) -> (Serializer<S>, Deserializer<D>)
where
    for<'a> D: Deserialize<'a>,
    S: Serialize,
{
    let (read, write) = conn.split();
    let wdelim = FramedWrite::new(write, FailureBytesCodec::new());
    let ser = WriteBincode::new(wdelim);
    let rdelim = FramedRead::new(read, FailureBytesCodec::new());
    let de = ReadBincode::new(rdelim);
    (ser, de)
}
