/*
 * Copyright 2019 OysterPack Inc.
 *
 *    Licensed under the Apache License, Version 2.0 (the "License");
 *    you may not use this file except in compliance with the License.
 *    You may obtain a copy of the License at
 *
 *        http://www.apache.org/licenses/LICENSE-2.0
 *
 *    Unless required by applicable law or agreed to in writing, software
 *    distributed under the License is distributed on an "AS IS" BASIS,
 *    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *    See the License for the specific language governing permissions and
 *    limitations under the License.
 */

//! Message package. Messages are designed to be highly secure.
//!
//! Messages are processed as streams, transitioning states while being processed.
//!
//! - when a peer connects, the initial message is the handshake.
//!   - each peer is identified by a public-key
//!   - the connecting peer plays the role of `client`; the peer being connected to plays the role of
//!     `server`
//!   - the client initiates a connection with a server by encrypting a `Connect` message using the
//!     server's public-key. Thus, only a specific server can decrypt the message.
//!   - the connect message contains a `PaymentChannel`
//!     - the client must commit funds in order to do business with the server
//!     - all payments are in Bitcoin
//!   - the client establishes a payment channel using secured funds
//!     - all payments are made via cryptocurrency
//!       - Bitcoin will initially be supported
//!       - payment is enforced via a smart contract
//!         - the smart contract defines the statement of work
//!         - funds are secured on a payment channel via a smart contract
//!         - the server provides proof of work to collect payment
//!         - when the connection is terminated, the server closes the contract and gets paid
//!           - change is returned to the client
//!     - each message contains a payment transaction
//!     - all messages processing fees are flat rates
//!       - a flat rate per unit of time for the connection
//!       - a flat rate per message byte
//!       - a flat rate for each message type
//!   - if the server successfully authenticates the client, then the server will reply with a
//!     `ConnectAccepted` reply
//!     - the message contains a shared secret cipher, which will be used to encrypt all future messages
//!       on this connection
//!       - the cipher expires and will be renewed by the server automatically
//!         - the server may push to the client a new cipher key. The client should switch over to using
//!           the new cipher key effective immediately
//!     - the message is hashed
//!     - the hash is digitally signed by the server
//!     - the message is encrypted using the client's private-key
//!
//! - when a peer comes online they register themselves with the services they provide
//!   - this enables clients to discover peers that offer services that the client is interested in
//!   - peers can advertise service metadata
//!     - service price
//!     - quality of service
//!     - capacity
//!     - hardware specs
//!     - smart contract
//!       - specifies message processing terms, prices, and payments
//!   - realtime metrics will be collected, which can help clients choose servers
//!   - clients can rate the server
//! - servers can blacklist clients that are submitting invalid requests
//! - clients can bid for services
//!   - clients can get immediate service if they pay the service ask price
//!   - clients can bid for a service at a lower price, sellers may choose to take the lower price
//!   - clients can bid higher, if service supply is low, in order to get higher priority
//!
//! ### Notes
//! - rmp_serde does not support Serde #[serde(skip_serializing_if="Option::is_none")] - it fails
//!   on deserialization - [https://github.com/3Hren/msgpack-rust/issues/86]
//!   - take away lesson is don't use the Serde #[serde(skip_serializing_if="Option::is_none")] feature
//!

use chrono::{DateTime, Duration, Utc};
use sodiumoxide::crypto::{box_, hash, secretbox, sign};
use flate2::bufread;
use oysterpack_errors::{Error, ErrorMessage, Id as ErrorId, IsError, Level as ErrorLevel};
use oysterpack_events::event::ModuleSource;
use oysterpack_uid::{Domain, DomainULID, ULID};
use std::{
    cmp, error, fmt,
    io::{self, Read, Write},
    time,
};

pub mod base58;
pub mod errors;
pub mod service;

/// Max message size - 256 KB
pub const MAX_MSG_SIZE: usize = 1000 * 256;

/// Min message size for SealedEnvelope using MessagePack encoding
pub const SEALED_ENVELOPE_MIN_SIZE: usize = 90;

/// A sealed envelope is secured via public-key authenticated encryption. It contains a private message
/// that is encrypted using the recipient's public-key and the sender's private-key. If the recipient
/// is able to decrypt the message, then the recipient knows it was sealed by the sender.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedEnvelope {
    sender: Address,
    recipient: Address,
    nonce: box_::Nonce,
    msg: EncryptedMessageBytes,
}

impl SealedEnvelope {
    /// decodes the io stream to construct a new SealedEnvelope
    /// - the stream must use the [bincode](https://crates.io/crates/bincode) encoding
    pub fn decode<R>(read: R) -> Result<SealedEnvelope, Error>
    where
        R: io::Read,
    {
        bincode::deserialize_from(read).map_err(|err| {
            op_error!(errors::MessageError::DecodingError(
                errors::DecodingError::InvalidSealedEnvelope(ErrorMessage(err.to_string()))
            ))
        })
    }

    /// encode the SealedEnvelope and write it to the io stream using [bincode](https://crates.io/crates/bincode) encoding
    pub fn encode<W: ?Sized>(&self, wr: &mut W) -> Result<(), Error>
    where
        W: io::Write,
    {
        bincode::serialize_into(wr, self).map_err(|err| {
            op_error!(errors::MessageError::EncodingError(
                errors::EncodingError::InvalidSealedEnvelope(ErrorMessage(err.to_string()))
            ))
        })
    }

    /// constructor
    pub fn new(
        sender: Address,
        recipient: Address,
        nonce: box_::Nonce,
        msg: &[u8],
    ) -> SealedEnvelope {
        SealedEnvelope {
            sender,
            recipient,
            nonce,
            msg: EncryptedMessageBytes(msg.into()),
        }
    }

    // TODO: implement TryFrom when it bocomes stable
    /// Converts an nng:Message into a SealedEnvelope.
    pub fn try_from_nng_message(msg: nng::Message) -> Result<SealedEnvelope, Error> {
        SealedEnvelope::decode(&**msg).map_err(|err| {
            op_error!(errors::NngMessageError::from(ErrorMessage::from(
                "Failed to decode SealedEnvelope"
            )))
            .with_cause(err)
        })
    }

    // TODO: implement TryInto when it becomes stable
    /// Converts itself into an nng:Message
    pub fn try_into_nng_message(self) -> Result<nng::Message, Error> {
        let bytes = bincode::serialize(&self).map_err(|err| {
            op_error!(errors::MessageError::EncodingError(
                errors::EncodingError::InvalidSealedEnvelope(ErrorMessage(err.to_string()))
            ))
        })?;
        let mut msg = nng::Message::with_capacity(bytes.len()).map_err(|err| {
            op_error!(errors::NngMessageError::from(ErrorMessage(format!("Failed to create an empty message with a pre-allocated body buffer (capacity = {}): {}", bytes.len(), err))))
        })?;
        msg.push_back(&bytes).map_err(|err| {
            op_error!(errors::NngMessageError::from(ErrorMessage(format!(
                "Failed to append data to the back of the message body: {}",
                err
            ))))
        })?;
        Ok(msg)
    }

    /// open the envelope using the specified precomputed key
    pub fn open(self, key: &box_::PrecomputedKey) -> Result<OpenEnvelope, Error> {
        match box_::open_precomputed(&self.msg.0, &self.nonce, key) {
            Ok(msg) => Ok(OpenEnvelope {
                sender: self.sender,
                recipient: self.recipient,
                msg: MessageBytes(msg),
            }),
            Err(_) => Err(op_error!(errors::SealedEnvelopeOpenFailed(&self))),
        }
    }

    /// msg bytes
    pub fn msg(&self) -> &[u8] {
        &self.msg.0
    }

    /// returns the sender address
    pub fn sender(&self) -> &Address {
        &self.sender
    }

    /// returns the recipient address
    pub fn recipient(&self) -> &Address {
        &self.recipient
    }

    /// returns the nonce
    pub fn nonce(&self) -> &box_::Nonce {
        &self.nonce
    }
}

impl fmt::Display for SealedEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} -> {}, nonce: {}, msg.len: {}",
            self.sender,
            self.recipient,
            base58::encode(&self.nonce.0),
            self.msg.0.len()
        )
    }
}

/// Represents an envelope that is open, i.e., its message is not encrypted
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenEnvelope {
    sender: Address,
    recipient: Address,
    msg: MessageBytes,
}

impl OpenEnvelope {
    /// constructor
    pub fn new(sender: Address, recipient: Address, msg: &[u8]) -> OpenEnvelope {
        OpenEnvelope {
            sender,
            recipient,
            msg: MessageBytes(msg.into()),
        }
    }

    /// seals the envelope
    pub fn seal(self, key: &box_::PrecomputedKey) -> SealedEnvelope {
        let nonce = box_::gen_nonce();
        SealedEnvelope {
            sender: self.sender,
            recipient: self.recipient,
            nonce,
            msg: EncryptedMessageBytes(box_::seal_precomputed(&self.msg.0, &nonce, key)),
        }
    }

    /// msg bytes
    pub fn msg(&self) -> &[u8] {
        &self.msg.0
    }

    /// returns the sender address
    pub fn sender(&self) -> &Address {
        &self.sender
    }

    /// returns the recipient address
    pub fn recipient(&self) -> &Address {
        &self.recipient
    }

    /// parses the message data into an encoded message
    pub fn encoded_message(self) -> Result<EncodedMessage, Error> {
        let msg: Message<MessageBytes> = bincode::deserialize(self.msg()).map_err(|err| {
            op_error!(errors::MessageError::MessageDataDeserializationFailed(
                &self.sender,
                errors::ErrorInfo(err.to_string())
            ))
        })?;
        Ok(EncodedMessage {
            sender: self.sender,
            recipient: self.recipient,
            msg,
        })
    }
}

impl fmt::Display for OpenEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} -> {}, msg.len: {}",
            self.sender,
            self.recipient,
            self.msg.0.len()
        )
    }
}

/// Addresses are identified by public-keys.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct Address(box_::PublicKey);

impl Address {
    /// returns the underlying public-key
    pub fn public_key(&self) -> &box_::PublicKey {
        &self.0
    }

    /// precompute the key that can be used to seal the envelope by the sender
    pub fn precompute_sealing_key(
        &self,
        sender_private_key: &box_::SecretKey,
    ) -> box_::PrecomputedKey {
        box_::precompute(&self.0, sender_private_key)
    }

    /// precompute the key that can be used to open the envelope by the recipient
    pub fn precompute_opening_key(
        &self,
        recipient_private_key: &box_::SecretKey,
    ) -> box_::PrecomputedKey {
        box_::precompute(&self.0, recipient_private_key)
    }
}

impl From<box_::PublicKey> for Address {
    fn from(address: box_::PublicKey) -> Address {
        Address(address)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", base58::encode(&(self.0).0))
    }
}

/// message data bytes that is encrypted
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct EncryptedMessageBytes(Vec<u8>);

impl EncryptedMessageBytes {
    /// returns the message bytess
    pub fn data(&self) -> &[u8] {
        &self.0
    }
}

impl From<&[u8]> for EncryptedMessageBytes {
    fn from(bytes: &[u8]) -> EncryptedMessageBytes {
        EncryptedMessageBytes(Vec::from(bytes))
    }
}

impl From<Vec<u8>> for EncryptedMessageBytes {
    fn from(bytes: Vec<u8>) -> EncryptedMessageBytes {
        EncryptedMessageBytes(bytes)
    }
}

impl std::iter::FromIterator<u8> for EncryptedMessageBytes {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = u8>,
    {
        EncryptedMessageBytes(Vec::from_iter(iter))
    }
}

/// message data bytes
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct MessageBytes(Vec<u8>);

impl MessageBytes {
    /// returns the message bytess
    pub fn data(&self) -> &[u8] {
        &self.0
    }

    /// hashes the message data
    pub fn hash(&self) -> hash::Digest {
        hash::hash(&self.0)
    }
}

impl From<&[u8]> for MessageBytes {
    fn from(bytes: &[u8]) -> MessageBytes {
        MessageBytes(Vec::from(bytes))
    }
}

impl From<Vec<u8>> for MessageBytes {
    fn from(bytes: Vec<u8>) -> MessageBytes {
        MessageBytes(bytes)
    }
}

impl std::iter::FromIterator<u8> for MessageBytes {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = u8>,
    {
        MessageBytes(Vec::from_iter(iter))
    }
}

/// Message metadata
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct Metadata {
    msg_type: MessageType,
    instance_id: InstanceId,
    encoding: Encoding,
    deadline: Option<Deadline>,
    correlation_id: Option<InstanceId>,
    session_id: SessionId,
    sequence: Option<Sequence>,
}

impl Metadata {
    /// constructor
    pub fn new(msg_type: MessageType, encoding: Encoding, deadline: Option<Deadline>) -> Metadata {
        Metadata {
            msg_type,
            instance_id: InstanceId::generate(),
            encoding,
            deadline,
            correlation_id: None,
            session_id: SessionId::generate(),
            sequence: None,
        }
    }

    /// sets the session id
    pub fn set_session_id(self, session_id: SessionId) -> Metadata {
        let mut md = self;
        md.session_id = session_id;
        md
    }

    /// sets the sequence
    pub fn set_sequence(self, sequence: Sequence) -> Metadata {
        let mut md = self;
        md.sequence = Some(sequence);
        md
    }

    /// correlate this message instance with another message instance, e.g., used to correlate a response
    /// message with a request message
    pub fn correlate(self, instance_id: InstanceId) -> Metadata {
        let mut md = self;
        md.correlation_id = Some(instance_id);
        md
    }

    /// correlation ID getter
    pub fn correlation_id(&self) -> Option<InstanceId> {
        self.correlation_id
    }

    /// Each message type is identified by an Id
    pub fn message_type(&self) -> MessageType {
        self.msg_type
    }

    /// Each message instance is assigned a unique ULID.
    /// - This could be used as a nonce for replay protection on the network.
    pub fn instance_id(&self) -> InstanceId {
        self.instance_id
    }

    /// When the message was created. This is derived from the message instance ID.
    ///
    /// ## NOTES
    /// The timestamp has millisecond granularity. If sub-millisecond granularity is required, then
    /// a numeric sequence based nonce would be required.
    pub fn timestamp(&self) -> DateTime<Utc> {
        self.instance_id.ulid().datetime()
    }

    /// A message can specify that it must be processed by the specified deadline.
    pub fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// return the message data encoding
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Message sequence is relative to the current session.
    ///
    /// No message sequence implies that messages can be processed in any order.
    ///
    /// ## Use Cases
    /// 1. The client-server protocol can use the sequence to strictly process messages in order.
    ///    For example, if the client sends a message with sequence=2, the sequence=2 message will
    ///    not be processed until the server knows that sequence=1 message had been processed.
    ///    The sequence=2 message will be held until sequence=1 message is received. The sequencing
    ///    protocol can be negotiated between the client and server.
    pub fn sequence(&self) -> Option<Sequence> {
        self.sequence
    }

    /// Each message is associated with a session
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }
}

/// Message sequence
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum Sequence {
    /// Messages must be processed in order relative to its session, e.g. Strict(2) message will not be processed until
    /// Strict(1) message is processed. Strict(2) message will be held until Strict(1) message
    /// is received, or until Strict(2) expires according to its deadline.
    Strict(u64),
    /// Messages are processed in order relative to its session. If the message received has a higher Loose(N)
    /// than the last message processed for the client session, then it will be processed. Otherwise,
    /// the message will be rejected as a stale message.
    Loose(u64),
}

impl Sequence {
    /// increments the sequence value
    pub fn inc(self) -> Sequence {
        match self {
            Sequence::Strict(n) => Sequence::Strict(n + 1),
            Sequence::Loose(n) => Sequence::Loose(n + 1),
        }
    }
}

/// Used to attach the MessageId to the message type
pub trait IsMessage {
    /// MessageTypeId is defined on the message type
    const MESSAGE_TYPE_ID: MessageTypeId;
}

/// Compression mode
#[derive(Debug, Serialize, Deserialize, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Compression {
    /// deflate
    Deflate,
    /// zlib
    Zlib,
    /// gzip
    Gzip,
    /// snappy
    Snappy,
    /// LZ4
    Lz4,
}

impl Compression {
    /// compress the data
    pub fn compress(self, data: &[u8]) -> io::Result<Vec<u8>> {
        match self {
            Compression::Deflate => {
                let mut deflater = bufread::DeflateEncoder::new(data, flate2::Compression::fast());
                let mut buffer = Vec::new();
                deflater.read_to_end(&mut buffer)?;
                Ok(buffer)
            }
            Compression::Zlib => {
                let mut deflater = bufread::ZlibEncoder::new(data, flate2::Compression::fast());
                let mut buffer = Vec::new();
                deflater.read_to_end(&mut buffer)?;
                Ok(buffer)
            }
            Compression::Gzip => {
                let mut deflater = bufread::GzEncoder::new(data, flate2::Compression::fast());
                let mut buffer = Vec::new();
                deflater.read_to_end(&mut buffer)?;
                Ok(buffer)
            }
            Compression::Snappy => Ok(parity_snappy::compress(data)),
            Compression::Lz4 => {
                let mut buf = Vec::with_capacity(data.len() / 2);
                let mut encoder = lz4::EncoderBuilder::new().build(&mut buf)?;
                encoder.write_all(data)?;
                let (_, result) = encoder.finish();
                match result {
                    Ok(_) => Ok(buf),
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// compress the data
    pub fn decompress(self, data: &[u8]) -> io::Result<Vec<u8>> {
        match self {
            Compression::Deflate => {
                let mut inflater = bufread::DeflateDecoder::new(data);
                let mut buffer = Vec::new();
                inflater.read_to_end(&mut buffer)?;
                Ok(buffer)
            }
            Compression::Zlib => {
                let mut inflater = bufread::ZlibDecoder::new(data);
                let mut buffer = Vec::new();
                inflater.read_to_end(&mut buffer)?;
                Ok(buffer)
            }
            Compression::Gzip => {
                let mut inflater = bufread::GzDecoder::new(data);
                let mut buffer = Vec::new();
                inflater.read_to_end(&mut buffer)?;
                Ok(buffer)
            }
            Compression::Snappy => parity_snappy::decompress(data)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            Compression::Lz4 => {
                let mut buf = Vec::with_capacity(data.len() / 2);
                let mut decoder = lz4::Decoder::new(data)?;
                io::copy(&mut decoder, &mut buf)?;
                Ok(buf)
            }
        }
    }
}

/// Message encoding format
///
/// ## Performance (based on simple benchmark tests)
/// - bincode tends to be the smallest
/// - based on some simple benchmarks, bincode+snappy are the fastest
///
/// You will want to benchmark to see what works best for your use case.
///
/// ## Notes
/// - [MessagePack](https://crates.io/crates/rmp-serde) was dropped because it was found to be too buggy
///   - there are issues deserializing Option(s), which is a show stopper
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub enum Encoding {
    /// [Bincode](https://github.com/TyOverby/bincode)
    Bincode(Option<Compression>),
    /// [CBOR](http://cbor.io/)
    CBOR(Option<Compression>),
    /// [JSON](https://www.json.org/)
    JSON(Option<Compression>),
}

impl Encoding {
    /// encode the data
    pub fn encode<T>(self, data: T) -> Result<Vec<u8>, Error>
    where
        T: serde::Serialize,
    {
        let (data, compression) = match self {
            Encoding::Bincode(compression) => {
                let data = bincode::serialize(&data)
                    .map_err(|err| op_error!(errors::SerializationError::new(self, err)))?;
                (data, compression)
            }
            Encoding::CBOR(compression) => {
                let data = serde_cbor::to_vec(&data)
                    .map_err(|err| op_error!(errors::SerializationError::new(self, err)))?;
                (data, compression)
            }
            Encoding::JSON(compression) => {
                let data = serde_json::to_vec(&data)
                    .map_err(|err| op_error!(errors::SerializationError::new(self, err)))?;
                (data, compression)
            }
        };

        if let Some(compression) = compression {
            compression
                .compress(&data)
                .map_err(|err| op_error!(errors::SerializationError::new(self, err)))
        } else {
            Ok(data)
        }
    }

    /// decodes the data
    pub fn decode<T>(self, data: &[u8]) -> Result<T, Error>
    where
        T: serde::de::DeserializeOwned,
    {
        match self {
            Encoding::Bincode(compression) => {
                if let Some(compression) = compression {
                    compression
                        .decompress(data)
                        .and_then(|data| {
                            bincode::deserialize(&data)
                                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
                        })
                        .map_err(|err| op_error!(errors::DeserializationError::new(self, err)))
                } else {
                    bincode::deserialize(data)
                        .map_err(|err| op_error!(errors::DeserializationError::new(self, err)))
                }
            }
            Encoding::CBOR(compression) => {
                if let Some(compression) = compression {
                    compression
                        .decompress(data)
                        .and_then(|data| {
                            serde_cbor::from_slice(&data)
                                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
                        })
                        .map_err(|err| op_error!(errors::DeserializationError::new(self, err)))
                } else {
                    serde_cbor::from_slice(data)
                        .map_err(|err| op_error!(errors::DeserializationError::new(self, err)))
                }
            }
            Encoding::JSON(compression) => {
                if let Some(compression) = compression {
                    compression
                        .decompress(data)
                        .and_then(|data| {
                            serde_json::from_slice(&data)
                                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
                        })
                        .map_err(|err| op_error!(errors::DeserializationError::new(self, err)))
                } else {
                    serde_json::from_slice(data)
                        .map_err(|err| op_error!(errors::DeserializationError::new(self, err)))
                }
            }
        }
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Encoding::Bincode(compression) => write!(f, "Bincode({:?})", compression),
            Encoding::CBOR(compression) => write!(f, "CBOR({:?})", compression),
            Encoding::JSON(compression) => write!(f, "JSON({:?})", compression),
        }
    }
}

/// Deadline
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Deadline {
    /// Max time allowed for the message to process
    ProcessingTimeoutMillis(u64),
    /// Message timeout is relative to the message timestamp
    MessageTimeoutMillis(u64),
}

impl Deadline {
    /// converts the deadline into a timeout duration
    /// - the starting_time is used when the deadline is Deadline::MessageTimeoutMillis. The timeout
    ///   is taken relative to the specified starting time. If the deadline time has passed, then
    ///   a zero duration is returned.
    pub fn duration(&self, starting_time: chrono::DateTime<Utc>) -> chrono::Duration {
        match self {
            Deadline::ProcessingTimeoutMillis(millis) => {
                chrono::Duration::milliseconds(*millis as i64)
            }
            Deadline::MessageTimeoutMillis(millis) => {
                let now = Utc::now();
                if starting_time >= now {
                    return chrono::Duration::zero();
                }
                let deadline = starting_time
                    .checked_add_signed(chrono::Duration::milliseconds(*millis as i64));
                match deadline {
                    Some(deadline) => {
                        if now >= deadline {
                            chrono::Duration::zero()
                        } else {
                            deadline.signed_duration_since(now)
                        }
                    }
                    None => chrono::Duration::zero(),
                }
            }
        }
    }
}

#[oysterpack_uid::macros::ulid]
/// Unique message type identifier
/// - MessageTypeId enables MessageType(s) to be defined as constants
pub struct MessageTypeId(pub u128);

impl MessageTypeId {
    /// converts itself into a MessageType
    pub fn message_type(&self) -> MessageType {
        MessageType(self.ulid())
    }
}

/// Identifies the message type, which tells us how to decode the bytes message data.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct MessageType(ULID);

impl From<MessageTypeId> for MessageType {
    fn from(type_id: MessageTypeId) -> MessageType {
        MessageType(type_id.ulid())
    }
}

/// MessageType Domain
pub const MESSAGE_TYPE_DOMAIN: Domain = Domain("MessageType");
/// MessageType InstanceId
pub const MESSAGE_INSTANCE_ID_DOMAIN: Domain = Domain("MessageInstanceId");

impl MessageType {
    /// ULID getter
    pub fn ulid(&self) -> ULID {
        self.0
    }

    /// represents itself as a DomainULID
    pub fn domain_ulid(&self) -> DomainULID {
        DomainULID::from_ulid(MESSAGE_TYPE_DOMAIN, self.0)
    }
}

impl fmt::Display for MessageType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Message instance unique identifier.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct InstanceId(ULID);

impl InstanceId {
    /// generates a new MessageInstance
    pub fn generate() -> InstanceId {
        InstanceId(ULID::generate())
    }

    /// ULID getter
    pub fn ulid(&self) -> ULID {
        self.0
    }

    /// represents itself as a DomainULID
    pub fn domain_ulid(&self) -> DomainULID {
        DomainULID::from_ulid(MESSAGE_INSTANCE_ID_DOMAIN, self.0)
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Encoded message data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message<T>
where
    T: fmt::Debug + Clone,
{
    metadata: Metadata,
    data: T,
}

impl<T> Message<T>
where
    T: fmt::Debug + Clone + serde::Serialize,
{
    /// constructor
    pub fn new(metadata: Metadata, data: T) -> Message<T> {
        Message { metadata, data }
    }

    /// returns the message metadata
    pub fn metadata(&self) -> Metadata {
        self.metadata
    }

    /// returns the message data
    pub fn data(&self) -> &T {
        &self.data
    }

    /// encode the message data into bytes
    pub fn encode(self) -> Result<Message<MessageBytes>, Error> {
        match self.metadata.encoding.encode(self.data) {
            Ok(data) => Ok(Message {
                metadata: self.metadata,
                data: MessageBytes(data),
            }),
            Err(err) => Err(err),
        }
    }

    /// converts itself into an EncodedMessage
    pub fn encoded_message(
        self,
        sender: Address,
        recipient: Address,
    ) -> Result<EncodedMessage, Error> {
        Ok(EncodedMessage {
            sender,
            recipient,
            msg: self.encode()?,
        })
    }

    /// Creates a new event, tagging it with the following domain tags:
    /// - MessageType
    /// - MessageInstanceId
    ///
    /// This links the event to the message.
    pub fn event<E>(&self, event: E, mod_src: ModuleSource) -> oysterpack_events::Event<E>
    where
        E: oysterpack_events::Eventful,
    {
        event
            .new_event(mod_src)
            .with_tag_id(self.metadata.message_type().domain_ulid())
            .with_tag_id(self.metadata.instance_id().domain_ulid())
    }
}

impl Message<MessageBytes> {
    /// converts the MessageBytes data to the specified type, based on the message metatdata
    pub fn decode<T>(self) -> Result<Message<T>, Error>
    where
        T: fmt::Debug + Clone + serde::de::DeserializeOwned + serde::Serialize,
    {
        match self.metadata.encoding.decode::<T>(self.data.data()) {
            Ok(data) => Ok(Message::new(self.metadata, data)),
            Err(err) => Err(err),
        }
    }
}

/// Encoded message data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodedMessage {
    sender: Address,
    recipient: Address,
    msg: Message<MessageBytes>,
}

impl EncodedMessage {
    /// sender event attribute ID (01CZ65BTACD5RCJAQFH09RFCDE)
    pub const EVENT_ATTR_ID_SENDER: oysterpack_events::AttributeId =
        oysterpack_events::AttributeId(1868178990369345351553440989424628142);
    /// sender event attribute ID (01CZ65DVD1J4C7Z9QKY586G9S5)
    pub const EVENT_ATTR_ID_RECIPIENT: oysterpack_events::AttributeId =
        oysterpack_events::AttributeId(1868179070938393865818884425430607653);

    /// returns the message metadata
    pub fn metadata(&self) -> Metadata {
        self.msg.metadata
    }

    /// returns the message data
    pub fn data(&self) -> &MessageBytes {
        &self.msg.data
    }

    /// return the sender's address
    pub fn sender(&self) -> &Address {
        &self.sender
    }

    /// return the recipient's address
    pub fn recipient(&self) -> &Address {
        &self.sender
    }

    /// converts into an OpenEnvelope
    pub fn open_envelope(self) -> Result<OpenEnvelope, Error> {
        let msg = MessageBytes(bincode::serialize(&self.msg).map_err(|err| {
            op_error!(errors::MessageError::EncodedMessageSerializationFailed(
                self.sender(),
                errors::ErrorInfo(err.to_string())
            ))
        })?);
        Ok(OpenEnvelope {
            sender: self.sender,
            recipient: self.recipient,
            msg,
        })
    }

    /// converts the MessageBytes data to the specified type, based on the message metatdata
    pub fn decode<T>(self) -> Result<(Addresses, Message<T>), Error>
    where
        T: fmt::Debug + Clone + serde::de::DeserializeOwned + serde::Serialize,
    {
        match self.msg.decode() {
            Ok(msg) => Ok((Addresses::new(self.sender, self.recipient), msg)),
            Err(err) => Err(err),
        }
    }

    /// constructor which encodes the specified message
    pub fn encode<T>(addresses: Addresses, msg: Message<T>) -> Result<EncodedMessage, Error>
    where
        T: fmt::Debug + Clone + serde::Serialize,
    {
        msg.encoded_message(addresses.sender, addresses.recipient)
    }

    /// Creates a new event, tagging it with the following domain tags:
    /// - MessageType
    /// - MessageInstanceId
    ///
    /// and with the following attributes:
    /// - Self::EVENT_ATTR_ID_SENDER
    /// - Self::EVENT_ATTR_ID_RECIPIENT
    ///
    /// This links the event to the message.
    pub fn event<E>(&self, event: E, mod_src: ModuleSource) -> oysterpack_events::Event<E>
    where
        E: oysterpack_events::Eventful,
    {
        self.msg
            .event(event, mod_src)
            .with_attribute(Self::EVENT_ATTR_ID_SENDER, self.sender)
            .with_attribute(Self::EVENT_ATTR_ID_RECIPIENT, self.recipient)
    }
}

/// Envelope addresses contain the sender's and recipient's addresses
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct Addresses {
    sender: Address,
    recipient: Address,
}

impl Addresses {
    /// constructor
    pub fn new(sender: Address, recipient: Address) -> Addresses {
        Addresses { sender, recipient }
    }

    /// sender address
    pub fn sender(&self) -> &Address {
        &self.sender
    }

    /// recipient address
    pub fn recipient(&self) -> &Address {
        &self.recipient
    }
}

/// Each new client connection is assigned a new SessionId
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct SessionId(ULID);

impl SessionId {
    /// constructor
    pub fn generate() -> SessionId {
        SessionId(ULID::generate())
    }

    /// session ULID
    pub fn ulid(&self) -> ULID {
        self.0
    }
}

impl From<ULID> for SessionId {
    fn from(ulid: ULID) -> SessionId {
        SessionId(ulid)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Encrypted digitally signed hash
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct EncryptedSignedHash(Vec<u8>, secretbox::Nonce);

impl EncryptedSignedHash {
    /// decrypts the signed hash and verifies the signature
    pub fn verify(
        &self,
        key: &secretbox::Key,
        public_key: &sign::PublicKey,
    ) -> Result<hash::Digest, Error> {
        match secretbox::open(&self.0, &self.1, key) {
            Ok(signed_hash) => match sign::verify(&signed_hash, public_key) {
                Ok(digest) => match hash::Digest::from_slice(&digest) {
                    Some(digest) => Ok(digest),
                    None => Err(op_error!(errors::MessageError::InvalidDigestLength {
                        from: public_key,
                        len: digest.len()
                    })),
                },
                Err(_) => Err(op_error!(errors::MessageError::InvalidSignature(
                    public_key
                ))),
            },
            Err(_) => Err(op_error!(errors::MessageError::DecryptionFailed(
                public_key
            ))),
        }
    }

    /// return the nonce used to encrypt this signed hash
    pub fn nonce(&self) -> &secretbox::Nonce {
        &self.1
    }
}

/// A digitally signed hash
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct SignedHash(Vec<u8>);

impl SignedHash {
    /// constructor - signs the hash using the specified private-key
    pub fn sign(digest: &hash::Digest, key: &sign::SecretKey) -> SignedHash {
        SignedHash(sign::sign(&digest.0, key))
    }

    /// verifies the hash's signature against the specified PublicKey, and then verifies the message
    /// integrity by checking its hash
    pub fn verify(&self, msg: &[u8], key: &sign::PublicKey) -> Result<(), Error> {
        let digest = sign::verify(&self.0, key)
            .map_err(|_| op_error!(errors::MessageError::InvalidSignature(key)))?;
        match hash::Digest::from_slice(&digest) {
            Some(digest) => {
                let msg_digest = hash::hash(msg);
                if msg_digest == digest {
                    Ok(())
                } else {
                    Err(op_error!(errors::MessageError::ChecksumFailed(key)))
                }
            }
            None => Err(op_error!(errors::MessageError::InvalidDigestLength {
                from: key,
                len: digest.len()
            })),
        }
    }

    /// encrypt the signed hash
    pub fn encrypt(&self, key: &secretbox::Key) -> EncryptedSignedHash {
        let nonce = secretbox::gen_nonce();
        EncryptedSignedHash(secretbox::seal(&self.0, &nonce, key), nonce)
    }
}

impl From<&[u8]> for SignedHash {
    fn from(bytes: &[u8]) -> SignedHash {
        SignedHash(Vec::from(bytes))
    }
}

impl From<Vec<u8>> for SignedHash {
    fn from(bytes: Vec<u8>) -> SignedHash {
        SignedHash(bytes)
    }
}

#[allow(warnings)]
#[cfg(test)]
mod test {
    use super::{
        base58, Address, EncryptedMessageBytes, MessageBytes, MessageType, OpenEnvelope,
        SealedEnvelope,
    };
    use crate::tests::run_test;
    use sodiumoxide::crypto::{box_, hash, secretbox, sign};
    use oysterpack_uid::ULID;
    use std::io;

    #[derive(Debug, Serialize, Deserialize)]
    struct Person {
        fname: String,
        lname: String,
    }

    #[test]
    fn deserialize_byte_stream_using_bincode() {
        let p1 = Person {
            fname: "Alfio".to_string(),
            lname: "Zappala".to_string(),
        };
        let p2 = Person {
            fname: "Andreas".to_string(),
            lname: "Antonopoulos".to_string(),
        };

        let mut p1_bytes = bincode::serialize(&p1).map_err(|_| ()).unwrap();
        let mut p2_bytes = bincode::serialize(&p2).map_err(|_| ()).unwrap();
        let p1_bytes_len = p1_bytes.len();
        p1_bytes.append(&mut p2_bytes);
        let bytes = p1_bytes.as_slice();
        let p1: Person = bincode::deserialize_from(bytes).unwrap();
        println!("p1: {:?}", p1);
        let p2: Person = bincode::deserialize_from(&bytes[p1_bytes_len..]).unwrap();
        println!("p2: {:?}", p2);
    }

    #[test]
    fn seal_open_envelope() {
        let (client_pub_key, client_priv_key) = box_::gen_keypair();
        let (server_pub_key, server_priv_key) = box_::gen_keypair();

        let (client_addr, server_addr) =
            (Address::from(client_pub_key), Address::from(server_pub_key));
        let opening_key = client_addr.precompute_opening_key(&server_priv_key);
        let sealing_key = server_addr.precompute_sealing_key(&client_priv_key);
        let msg = b"data";

        run_test("seal_open_envelope", || {
            info!("addresses: {} -> {}", client_addr, server_addr);
            let open_envelope =
                OpenEnvelope::new(client_pub_key.into(), server_pub_key.into(), msg);
            let open_envelope_rmp = bincode::serialize(&open_envelope).unwrap();
            info!("open_envelope_rmp len = {}", open_envelope_rmp.len());
            let sealed_envelope = open_envelope.seal(&sealing_key);
            let sealed_envelope_rmp = bincode::serialize(&sealed_envelope).unwrap();
            info!("sealed_envelope_rmp len = {}", sealed_envelope_rmp.len());
            info!(
                "sealed_envelope json: {}",
                serde_json::to_string_pretty(&sealed_envelope).unwrap()
            );
            info!("sealed_envelope msg len: {}", sealed_envelope.msg().len());

            let open_envelope_2 = sealed_envelope.open(&opening_key).unwrap();
            info!(
                "open_envelope_2 json: {}",
                serde_json::to_string_pretty(&open_envelope_2).unwrap()
            );
            info!("open_envelope_2 msg len: {}", open_envelope_2.msg().len());
            assert_eq!(*open_envelope_2.msg(), *msg);
        });

        let msg = &[0 as u8; 1000 * 256];
        let msg = &msg[..];
        let open_envelope = OpenEnvelope::new(client_pub_key.into(), server_pub_key.into(), msg);
        run_test("seal_envelope", || {
            let _ = open_envelope.seal(&sealing_key);
        });

        let open_envelope = OpenEnvelope::new(client_pub_key.into(), server_pub_key.into(), msg);
        let sealed_envelope = open_envelope.seal(&sealing_key);
        run_test("open_envelope", || {
            let _ = sealed_envelope.open(&opening_key).unwrap();
        });
    }

    #[test]
    fn sealed_envelope_nng_conversions() {
        let (client_pub_key, client_priv_key) = box_::gen_keypair();
        let (server_pub_key, server_priv_key) = box_::gen_keypair();
        let (client_addr, server_addr) =
            (Address::from(client_pub_key), Address::from(server_pub_key));
        let opening_key = client_addr.precompute_opening_key(&server_priv_key);
        let sealing_key = server_addr.precompute_sealing_key(&client_priv_key);
        let msg = b"data";

        let open_envelope = OpenEnvelope::new(client_pub_key.into(), server_pub_key.into(), msg);
        let sealed_envelope = open_envelope.seal(&sealing_key);
        let nng_msg = sealed_envelope.try_into_nng_message().unwrap();
        let sealed_envelope = SealedEnvelope::try_from_nng_message(nng_msg).unwrap();
        let open_envelope = sealed_envelope.open(&opening_key).unwrap();
        assert_eq!(open_envelope.msg(), msg);
    }

    #[test]
    fn sealed_envelope_nng_aio_messaging() {
        use nng::{
            aio::{Aio, Context},
            Message, Protocol, Socket,
        };
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use std::{env, thread};

        /// Number of outstanding requests that we can handle at a given time.
        ///
        /// This is *NOT* the number of threads in use, but instead represents
        /// outstanding work items. Select a small number to reduce memory size. (Each
        /// one of these can be thought of as a request-reply loop.) Note that you will
        /// probably run into limitations on the number of open file descriptors if you
        /// set this too high. (If not for that limit, this could be set in the
        /// thousands, each context consumes a couple of KB.)
        const PARALLEL: usize = 10;

        /// Run the server portion of the program.
        fn server(
            url: &str,
            start: mpsc::Sender<()>,
            shutdown: mpsc::Receiver<()>,
        ) -> Result<(), nng::Error> {
            // Create the socket
            let mut s = Socket::new(Protocol::Rep0)?;

            // Create all of the worker contexts
            let workers: Vec<_> = (0..PARALLEL)
                .map(|i| create_worker(i, &s))
                .collect::<Result<_, _>>()?;

            // Only after we have the workers do we start listening.
            s.listen(url)?;

            // Now start all of the workers listening.
            for (a, c) in &workers {
                a.recv(c)?;
            }

            start
                .send(())
                .expect("Failed to send server started signal");

            // block until server shutdown is signalled
            shutdown.recv();
            println!("server is shutting down ...");

            Ok(())
        }

        /// Create a new worker context for the server.
        fn create_worker(i: usize, s: &Socket) -> Result<(Aio, Context), nng::Error> {
            let mut state = State::Recv;

            let ctx = Context::new(s)?;
            let ctx_clone = ctx.clone();
            let aio =
                Aio::with_callback(move |aio| worker_callback(i, aio, &ctx_clone, &mut state))?;

            Ok((aio, ctx))
        }

        /// Callback function for workers.
        fn worker_callback(i: usize, aio: &Aio, ctx: &Context, state: &mut State) {
            let new_state = match *state {
                State::Recv => {
                    println!("[{}] state: {:?}", i, state);
                    // If there was an issue, we're just going to panic instead of
                    // doing something sensible.
                    let _ = aio.result().unwrap();
                    match aio.get_msg() {
                        Some(msg) => {
                            println!("[{}] received message: state: {:?}", i, state);
                            let sealed_envelope =
                                SealedEnvelope::try_from_nng_message(msg).unwrap();
                            // echo back the message
                            let response = sealed_envelope.try_into_nng_message().unwrap();
                            aio.send(ctx, response).unwrap();
                            State::Send
                        }
                        None => {
                            println!("[{}] no message: state: {:?}", i, state);
                            State::Recv
                        }
                    }
                }
                State::Send => {
                    println!("[{}] state: {:?}", i, state);
                    // Again, just panic bad if things happened.
                    let _ = aio.result().unwrap();
                    aio.recv(ctx).unwrap();

                    State::Recv
                }
            };

            *state = new_state;
        }

        /// State of a request.
        #[derive(Debug, Copy, Clone)]
        enum State {
            Recv,
            Send,
        }

        fn send_request(s: &mut Socket, msg: SealedEnvelope) -> Result<SealedEnvelope, nng::Error> {
            let req = msg.try_into_nng_message().unwrap();

            let start = Instant::now();
            s.send(req)?;
            let resp = s.recv()?;

            let dur = Instant::now().duration_since(start);
            println!("Request took {:?} milliseconds", dur);
            let resp = SealedEnvelope::try_from_nng_message(resp).unwrap();
            Ok(resp)
        }

        let mut s = Socket::new(Protocol::Req0).unwrap();
        let url = "inproc://test";

        let (server_shutdown_tx, server_shutdown_rx) = mpsc::channel();
        let (server_started_tx, server_started_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            server(url, server_started_tx, server_shutdown_rx).expect("Failed to start the server");
        });
        // wait until the server has started
        server_started_rx.recv();
        // connect to the server
        s.dial(url).unwrap();

        // create the message
        let (client_pub_key, client_priv_key) = box_::gen_keypair();
        let (server_pub_key, server_priv_key) = box_::gen_keypair();
        let (client_addr, server_addr) =
            (Address::from(client_pub_key), Address::from(server_pub_key));
        let opening_key = client_addr.precompute_opening_key(&server_priv_key);
        let sealing_key = server_addr.precompute_sealing_key(&client_priv_key);
        let msg = b"data";

        // send request messages
        for i in 0..100 {
            let start = Instant::now();
            let open_envelope = OpenEnvelope::new(client_pub_key.into(), server_pub_key.into(), msg);
            let sealed_envelope = open_envelope.seal(&sealing_key);
            let resp = send_request(&mut s, sealed_envelope).unwrap();
            let dur = Instant::now().duration_since(start);
            println!("Request[{}] took {:?} milliseconds", i, dur);
            let resp = resp.open(&opening_key).unwrap();
            assert_eq!(resp.msg(), msg);
        }

        // signal the server to shutdown
        server_shutdown_tx
            .send(())
            .expect("Failed to send shutdown signal");
        server.join().unwrap();
    }

    #[test]
    fn sealed_envelope_encoding_decoding() {
        let (client_pub_key, client_priv_key) = box_::gen_keypair();
        let (server_pub_key, server_priv_key) = box_::gen_keypair();

        let (client_addr, server_addr) =
            (Address::from(client_pub_key), Address::from(server_pub_key));
        let opening_key = client_addr.precompute_opening_key(&server_priv_key);
        let sealing_key = server_addr.precompute_sealing_key(&client_priv_key);

        run_test("sealed_envelope_encoding_decoding", || {
            info!("addresses: {} -> {}", client_addr, server_addr);
            let open_envelope =
                OpenEnvelope::new(client_pub_key.into(), server_pub_key.into(), b"");
            let mut sealed_envelope = open_envelope.seal(&sealing_key);

            let mut buf: io::Cursor<Vec<u8>> = io::Cursor::new(Vec::new());
            sealed_envelope.encode(&mut buf);
            info!(
                "SealedEnvelope[{}]: {:?} - {}",
                buf.get_ref().as_slice().len(),
                buf.get_ref().as_slice(),
                sealed_envelope.msg().len()
            );

            let sealed_envelope_decoded = SealedEnvelope::decode(buf.get_ref().as_slice()).unwrap();
            assert_eq!(sealed_envelope.sender(), sealed_envelope_decoded.sender());
            assert_eq!(
                sealed_envelope.recipient(),
                sealed_envelope_decoded.recipient()
            );

            sealed_envelope.msg = EncryptedMessageBytes(vec![1]);
            let mut buf: io::Cursor<Vec<u8>> = io::Cursor::new(Vec::new());
            sealed_envelope.encode(&mut buf);
            info!(
                "SealedEnvelope[{}]: {:?} - {}",
                buf.get_ref().as_slice().len(),
                buf.get_ref().as_slice(),
                sealed_envelope.msg().len()
            );
        });
    }

    #[test]
    fn base58_encoding_keys() {
        let (pub_key, priv_key) = box_::gen_keypair();

        let pub_key_base58 = base58::encode(&pub_key.0);
        let pub_key_bytes = base58::decode(&pub_key_base58).unwrap();
        let pub_key2 = box_::PublicKey::from_slice(&pub_key_bytes).unwrap();
        assert_eq!(pub_key, pub_key2);

        let key_base58 = base58::encode(&priv_key.0);
        let key_bytes = base58::decode(&key_base58).unwrap();
        let key2 = box_::SecretKey::from_slice(&key_bytes).unwrap();
        assert_eq!(priv_key, key2);
    }

    #[test]
    fn encrypted_signed_hash() {
        let (client_pub_key, client_priv_key) = sign::gen_keypair();
        let cipher = secretbox::gen_key();
        let session_id = super::SessionId::generate();

        let data = b"some data";
        let data_hash = hash::hash(data);
        let signed_hash_1 = super::SignedHash::sign(&data_hash, &client_priv_key);
        let encrypted_signed_hash_1 = signed_hash_1.encrypt(&cipher);
        let encrypted_signed_hash_2 = signed_hash_1.encrypt(&cipher);
        assert_ne!(
            encrypted_signed_hash_1.nonce(),
            encrypted_signed_hash_2.nonce(),
            "A new nonce should be used each time the signed session id is encrypted"
        );
        let digest_1 = encrypted_signed_hash_1
            .verify(&cipher, &client_pub_key)
            .unwrap();
        let digest_2 = encrypted_signed_hash_2
            .verify(&cipher, &client_pub_key)
            .unwrap();
        assert_eq!(digest_1, digest_2);
        assert_eq!(digest_1, data_hash);
    }

    #[test]
    fn test_message_bytes_deserialization() {
        #[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
        struct Foo(String);

        fn new_msg() -> super::Message<MessageBytes> {
            const MESSAGE_TYPE: super::MessageTypeId =
                super::MessageTypeId(1867384532653698871582487715619812439);
            let metadata = super::Metadata::new(
                MESSAGE_TYPE.message_type(),
                super::Encoding::Bincode(None),
                Some(super::Deadline::ProcessingTimeoutMillis(100)),
            );

            let data = super::MessageBytes::from(
                metadata.encoding().encode(&Foo("FOO".to_string())).unwrap(),
            );
            super::Message { metadata, data }
        }
        let msg = new_msg();
        let encoding = msg.metadata().encoding();

        // verify that msg can be serialized / deserialized
        let msg_bytes = encoding.encode(&msg).unwrap();
        if let Err(err) = encoding.decode::<super::Message<MessageBytes>>(&msg_bytes) {
            panic!("Failed to deserialize Message: {}", err);
        }
    }

    #[test]
    fn encoded_message() {
        let (client_pub_key, client_priv_key) = box_::gen_keypair();
        let (server_pub_key, server_priv_key) = box_::gen_keypair();

        let (client_addr, server_addr) =
            (Address::from(client_pub_key), Address::from(server_pub_key));
        let opening_key = client_addr.precompute_opening_key(&server_priv_key);
        let sealing_key = server_addr.precompute_sealing_key(&client_priv_key);

        #[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
        struct Foo(String);

        fn new_msg() -> super::Message<MessageBytes> {
            const MESSAGE_TYPE: super::MessageTypeId =
                super::MessageTypeId(1867384532653698871582487715619812439);
            let metadata = super::Metadata::new(
                MESSAGE_TYPE.message_type(),
                super::Encoding::Bincode(None),
                Some(super::Deadline::ProcessingTimeoutMillis(100)),
            );

            let data = super::MessageBytes::from(
                metadata.encoding().encode(&Foo("FOO".to_string())).unwrap(),
            );
            super::Message { metadata, data }
        }
        let msg = new_msg();
        let encoding = msg.metadata().encoding();

        // verify that msg can be serialized / deserialized
        let msg_bytes = encoding.encode(&msg).unwrap();
        if let Err(err) = encoding.decode::<super::Message<MessageBytes>>(&msg_bytes) {
            panic!("Failed to deserialize Message: {}", err);
        }

        // verify that the Message<MessageBytes> can be converted into Message<Foo>
        let foo_msg = msg.clone().decode::<Foo>().unwrap();
        assert_eq!(*foo_msg.data(), Foo("FOO".to_string()));

        run_test("sealed_envelope_encoding_decoding", || {
            info!("addresses: {} -> {}", client_addr, server_addr);
            let open_envelope = OpenEnvelope::new(
                client_pub_key.into(),
                server_pub_key.into(),
                &bincode::serialize(&msg).unwrap(),
            );
            let encoded_message = open_envelope.clone().encoded_message().unwrap();
            let open_envelope_2 = encoded_message.open_envelope().unwrap();
            assert_eq!(open_envelope.sender(), open_envelope_2.sender());
            assert_eq!(open_envelope.recipient(), open_envelope_2.recipient());
            assert_eq!(open_envelope.msg(), open_envelope_2.msg());

            let encoded_message = open_envelope.encoded_message().unwrap();
            let (addresses, msg) = encoded_message.clone().decode::<Foo>().unwrap();
            let encoded_message_2 = super::EncodedMessage::encode(addresses, msg).unwrap();
            assert_eq!(encoded_message.sender(), encoded_message_2.sender());
            assert_eq!(encoded_message.recipient(), encoded_message_2.recipient());
            assert_eq!(encoded_message.metadata(), encoded_message_2.metadata());
            assert_eq!(encoded_message.data(), encoded_message_2.data());
        });
    }

    #[test]
    fn bincode_encoded_message() {
        let (client_pub_key, client_priv_key) = box_::gen_keypair();
        let (server_pub_key, server_priv_key) = box_::gen_keypair();

        let (client_addr, server_addr) =
            (Address::from(client_pub_key), Address::from(server_pub_key));
        let opening_key = client_addr.precompute_opening_key(&server_priv_key);
        let sealing_key = server_addr.precompute_sealing_key(&client_priv_key);

        #[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
        struct Foo(String);

        fn new_msg() -> super::Message<MessageBytes> {
            const MESSAGE_TYPE: super::MessageTypeId =
                super::MessageTypeId(1867384532653698871582487715619812439);
            let metadata = super::Metadata::new(
                MESSAGE_TYPE.message_type(),
                super::Encoding::Bincode(None),
                None,
            );

            let data = super::MessageBytes::from(
                metadata.encoding().encode(&Foo("FOO".to_string())).unwrap(),
            );
            super::Message { metadata, data }
        }
        let msg = new_msg();
        let encoding = msg.metadata().encoding();

        // verify that msg can be serialized / deserialized
        let msg_bytes = encoding.encode(&msg).unwrap();
        if let Err(err) = encoding.decode::<super::Message<MessageBytes>>(&msg_bytes) {
            panic!("Failed to deserialize Message: {}", err);
        }

        // verify that the Message<MessageBytes> can be converted into Message<Foo>
        let foo_msg = msg.clone().decode::<Foo>().unwrap();
        assert_eq!(*foo_msg.data(), Foo("FOO".to_string()));

        run_test("sealed_envelope_encoding_decoding", || {
            info!("addresses: {} -> {}", client_addr, server_addr);
            let open_envelope = OpenEnvelope::new(
                client_pub_key.into(),
                server_pub_key.into(),
                &bincode::serialize(&msg).unwrap(),
            );
            let encoded_message = open_envelope.clone().encoded_message().unwrap();
            let open_envelope_2 = encoded_message.open_envelope().unwrap();
            assert_eq!(open_envelope.sender(), open_envelope_2.sender());
            assert_eq!(open_envelope.recipient(), open_envelope_2.recipient());
            assert_eq!(open_envelope.msg(), open_envelope_2.msg());

            let encoded_message = open_envelope.encoded_message().unwrap();
            let (addresses, msg) = encoded_message.clone().decode::<Foo>().unwrap();
            let encoded_message_2 = super::EncodedMessage::encode(addresses, msg).unwrap();
            assert_eq!(encoded_message.sender(), encoded_message_2.sender());
            assert_eq!(encoded_message.recipient(), encoded_message_2.recipient());
            assert_eq!(encoded_message.metadata(), encoded_message_2.metadata());
            assert_eq!(encoded_message.data(), encoded_message_2.data());
        });
    }

    #[test]
    fn bincode_compressed_encodings() {
        let (client_pub_key, client_priv_key) = box_::gen_keypair();
        let (server_pub_key, server_priv_key) = box_::gen_keypair();

        let addresses = super::Addresses::new(client_pub_key.into(), server_pub_key.into());

        use super::IsMessage;
        #[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
        struct Foo(String);
        impl IsMessage for Foo {
            const MESSAGE_TYPE_ID: super::MessageTypeId =
                super::MessageTypeId(1867384532653698871582487715619812439);
        }

        let foo = Foo("hello 1867384532653698871582487715619812439 1867384532653698871582487715619812439 1867384532653698871582487715619812439".to_string());

        run_test("bincode_compressed_encodings", || {
            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::Bincode(None),
                None,
            );
            let msg = super::Message::new(metadata, foo.clone());
            let msg = msg.encode().unwrap();
            info!("no compression msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::Bincode(Some(super::Compression::Deflate)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("deflate msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::Bincode(Some(super::Compression::Gzip)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("gzip msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::Bincode(Some(super::Compression::Zlib)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("zlib msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::Bincode(Some(super::Compression::Snappy)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("snappy msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::Bincode(Some(super::Compression::Lz4)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("lz4 msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);
        });
    }

    #[test]
    fn json_compressed_encodings() {
        let (client_pub_key, client_priv_key) = box_::gen_keypair();
        let (server_pub_key, server_priv_key) = box_::gen_keypair();

        let addresses = super::Addresses::new(client_pub_key.into(), server_pub_key.into());

        use super::IsMessage;
        #[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
        struct Foo(String);
        impl IsMessage for Foo {
            const MESSAGE_TYPE_ID: super::MessageTypeId =
                super::MessageTypeId(1867384532653698871582487715619812439);
        }

        let foo = Foo("hello 1867384532653698871582487715619812439 1867384532653698871582487715619812439 1867384532653698871582487715619812439".to_string());

        run_test("json_compressed_encodings", || {
            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::JSON(None),
                None,
            );
            let msg = super::Message::new(metadata, foo.clone());
            let msg = msg.encode().unwrap();
            info!("no compression msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::JSON(Some(super::Compression::Deflate)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("deflate msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::JSON(Some(super::Compression::Gzip)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("gzip msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::JSON(Some(super::Compression::Zlib)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("zlib msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::JSON(Some(super::Compression::Snappy)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("snappy msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::JSON(Some(super::Compression::Lz4)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("lz4 msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);
        });
    }

    #[test]
    fn cbor_compressed_encodings() {
        let (client_pub_key, client_priv_key) = box_::gen_keypair();
        let (server_pub_key, server_priv_key) = box_::gen_keypair();

        let addresses = super::Addresses::new(client_pub_key.into(), server_pub_key.into());

        use super::IsMessage;
        #[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
        struct Foo(String);
        impl IsMessage for Foo {
            const MESSAGE_TYPE_ID: super::MessageTypeId =
                super::MessageTypeId(1867384532653698871582487715619812439);
        }

        let foo = Foo("hello 1867384532653698871582487715619812439 1867384532653698871582487715619812439 1867384532653698871582487715619812439".to_string());

        run_test("cbor_compressed_encodings", || {
            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::CBOR(None),
                None,
            );
            let msg = super::Message::new(metadata, foo.clone());
            let msg = msg.encode().unwrap();
            info!("no compression msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::CBOR(Some(super::Compression::Deflate)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("deflate msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::CBOR(Some(super::Compression::Gzip)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("gzip msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::CBOR(Some(super::Compression::Zlib)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("zlib msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::CBOR(Some(super::Compression::Snappy)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("snappy msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);

            let metadata = super::Metadata::new(
                Foo::MESSAGE_TYPE_ID.message_type(),
                super::Encoding::CBOR(Some(super::Compression::Lz4)),
                None,
            );
            let msg = super::Message::new(metadata.clone(), foo.clone());
            let msg = msg.encode().unwrap();
            info!("lz4 msg size = {}", msg.data().data().len());
            let msg = msg.decode::<Foo>().unwrap();
            assert_eq!(*msg.data(), foo);
        });
    }

    #[test]
    fn deadline() {
        let start = chrono::Utc::now();

        let deadline = super::Deadline::ProcessingTimeoutMillis(100);
        assert_eq!(
            deadline.duration(start),
            chrono::Duration::milliseconds(100)
        );

        let deadline = super::Deadline::MessageTimeoutMillis(100);
        assert!(deadline.duration(start) <= chrono::Duration::milliseconds(100));

        let deadline = super::Deadline::MessageTimeoutMillis(100);
        let start = start
            .checked_sub_signed(chrono::Duration::milliseconds(200))
            .unwrap();
        assert_eq!(deadline.duration(start), chrono::Duration::zero());
    }
}
