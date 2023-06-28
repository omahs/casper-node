//! Incoming message parser.

mod multiframe;
mod outgoing_message;

use std::{collections::HashSet, num::NonZeroU32};

use bytes::{Buf, Bytes, BytesMut};
use thiserror::Error;

use self::{multiframe::MultiframeReceiver, outgoing_message::OutgoingMessage};
use crate::{
    header::{ErrorKind, Header, Kind},
    try_outcome, ChannelConfiguration, ChannelId, Id,
    Outcome::{self, Fatal, Incomplete, Success},
};

const UNKNOWN_CHANNEL: ChannelId = ChannelId::new(0);
const UNKNOWN_ID: Id = Id::new(0);

/// A parser/state machine that processes an incoming stream.
///
/// Does not handle IO, rather it expects a growing [`BytesMut`] buffer to be passed in, containing
/// incoming data.
#[derive(Debug)]
pub struct MessageReader<const N: usize> {
    /// Incoming channels
    channels: [Channel; N],
    max_frame_size: u32,
}

#[derive(Debug)]
struct Channel {
    incoming_requests: HashSet<Id>,
    outgoing_requests: HashSet<Id>,
    current_multiframe_receive: MultiframeReceiver,
    cancellation_allowance: u32,
    config: ChannelConfiguration,
}

impl Channel {
    #[inline]
    fn in_flight_requests(&self) -> u32 {
        self.incoming_requests.len() as u32
    }

    #[inline]
    fn is_at_max_requests(&self) -> bool {
        self.in_flight_requests() == self.config.request_limit
    }

    fn increment_cancellation_allowance(&mut self) {
        if self.cancellation_allowance < self.config.request_limit {
            self.cancellation_allowance += 1;
        }
    }
}

pub enum CompletedRead {
    ErrorReceived(Header),
    NewRequest { id: Id, payload: Option<Bytes> },
    ReceivedResponse { id: Id, payload: Option<Bytes> },
    RequestCancellation { id: Id },
    ResponseCancellation { id: Id },
}

#[derive(Copy, Clone, Debug, Error)]
pub enum LocalProtocolViolation {
    /// TODO: docs with hint what the programming error could be
    #[error("sending would exceed request limit")]
    WouldExceedRequestLimit,
    /// TODO: docs with hint what the programming error could be
    #[error("invalid channel")]
    InvalidChannel(ChannelId),
}

impl<const N: usize> MessageReader<N> {
    // TODO: Make return channel ref.
    #[inline(always)]
    fn channel_index(&self, channel: ChannelId) -> Result<usize, LocalProtocolViolation> {
        if channel.0 as usize >= N {
            Err(LocalProtocolViolation::InvalidChannel(channel))
        } else {
            Ok(channel.0 as usize)
        }
    }

    /// Returns whether or not it is permissible to send another request on given channel.
    #[inline]
    pub fn allowed_to_send_request(
        &self,
        channel: ChannelId,
    ) -> Result<bool, LocalProtocolViolation> {
        let chan_idx = self.channel_index(channel)?;
        let chan = &self.channels[chan_idx];

        Ok(chan.outgoing_requests.len() < chan.config.request_limit as usize)
    }

    /// Creates a new request to be sent.
    ///
    /// # Note
    ///
    /// Any caller of this functions should call `allowed_to_send_request()` before this function
    /// to ensure the channels request limit is not exceeded. Failure to do so may result in the
    /// peer closing the connection due to a protocol violation.
    pub fn create_request(
        &mut self,
        channel: ChannelId,
        payload: Option<Bytes>,
    ) -> Result<OutgoingMessage, LocalProtocolViolation> {
        let id = self.generate_request_id(channel);

        if !self.allowed_to_send_request(channel)? {
            return Err(LocalProtocolViolation::WouldExceedRequestLimit);
        }

        if let Some(payload) = payload {
            let header = Header::new(crate::header::Kind::RequestPl, channel, id);
            Ok(OutgoingMessage::new(header, Some(payload)))
        } else {
            let header = Header::new(crate::header::Kind::Request, channel, id);
            Ok(OutgoingMessage::new(header, None))
        }
    }

    /// Generate a new, unused request ID.
    fn generate_request_id(&mut self, channel: ChannelId) -> Id {
        todo!()
    }

    pub fn process_incoming(&mut self, mut buffer: BytesMut) -> Outcome<CompletedRead, Header> {
        // First, attempt to complete a frame.
        loop {
            // We do not have enough data to extract a header, indicate and return.
            if buffer.len() < Header::SIZE {
                return Incomplete(NonZeroU32::new((Header::SIZE - buffer.len()) as u32).unwrap());
            }

            let header_raw: [u8; Header::SIZE] = buffer[0..Header::SIZE].try_into().unwrap();
            let header = match Header::parse(header_raw) {
                Some(header) => header,
                None => {
                    // The header was invalid, return an error.
                    return Fatal(Header::new_error(
                        ErrorKind::InvalidHeader,
                        UNKNOWN_CHANNEL,
                        UNKNOWN_ID,
                    ));
                }
            };

            // We have a valid header, check if it is an error.
            if header.is_error() {
                match header.error_kind() {
                    ErrorKind::Other => {
                        // TODO: `OTHER` errors may contain a payload.

                        unimplemented!()
                    }
                    _ => {
                        return Success(CompletedRead::ErrorReceived(header));
                    }
                }
            }

            // At this point we are guaranteed a valid non-error frame, verify its channel.
            let channel = match self.channels.get_mut(header.channel().get() as usize) {
                Some(channel) => channel,
                None => return Fatal(header.with_err(ErrorKind::InvalidChannel)),
            };

            match header.kind() {
                Kind::Request => {
                    if channel.is_at_max_requests() {
                        return Fatal(header.with_err(ErrorKind::RequestLimitExceeded));
                    }

                    if channel.incoming_requests.insert(header.id()) {
                        return Fatal(header.with_err(ErrorKind::DuplicateRequest));
                    }
                    channel.increment_cancellation_allowance();

                    // At this point, we have a valid request and its ID has been added to our
                    // incoming set. All we need to do now is to remove it from the buffer.
                    buffer.advance(Header::SIZE);

                    return Success(CompletedRead::NewRequest {
                        id: header.id(),
                        payload: None,
                    });
                }
                Kind::Response => {
                    if !channel.outgoing_requests.remove(&header.id()) {
                        return Fatal(header.with_err(ErrorKind::FictitiousRequest));
                    } else {
                        return Success(CompletedRead::ReceivedResponse {
                            id: header.id(),
                            payload: None,
                        });
                    }
                }
                Kind::RequestPl => {
                    // First, we need to "gate" the incoming request; it only gets to bypass the request limit if it is already in progress:
                    let is_new_request = channel.current_multiframe_receive.is_new_transfer(header);

                    if is_new_request {
                        // If we're in the ready state, requests must be eagerly rejected if
                        // exceeding the limit.
                        if channel.is_at_max_requests() {
                            return Fatal(header.with_err(ErrorKind::RequestLimitExceeded));
                        }

                        // We also check for duplicate requests early to avoid reading them.
                        if channel.incoming_requests.contains(&header.id()) {
                            return Fatal(header.with_err(ErrorKind::DuplicateRequest));
                        }
                    };

                    let multiframe_outcome: Option<BytesMut> =
                        try_outcome!(channel.current_multiframe_receive.accept(
                            header,
                            &mut buffer,
                            self.max_frame_size,
                            channel.config.max_request_payload_size,
                            ErrorKind::RequestTooLarge
                        ));

                    // If we made it to this point, we have consumed the frame. Record it.
                    if is_new_request {
                        if channel.incoming_requests.insert(header.id()) {
                            return Fatal(header.with_err(ErrorKind::DuplicateRequest));
                        }
                        channel.increment_cancellation_allowance();
                    }

                    match multiframe_outcome {
                        Some(payload) => {
                            // Message is complete.
                            return Success(CompletedRead::NewRequest {
                                id: header.id(),
                                payload: Some(payload.freeze()),
                            });
                        }
                        None => {
                            // We need more frames to complete the payload. Do nothing and attempt
                            // to read the next frame.
                        }
                    }
                }
                Kind::ResponsePl => {
                    let is_new_response =
                        channel.current_multiframe_receive.is_new_transfer(header);

                    // Ensure it is not a bogus response.
                    if is_new_response {
                        if !channel.outgoing_requests.contains(&header.id()) {
                            return Fatal(header.with_err(ErrorKind::FictitiousRequest));
                        }
                    }

                    let multiframe_outcome: Option<BytesMut> =
                        try_outcome!(channel.current_multiframe_receive.accept(
                            header,
                            &mut buffer,
                            self.max_frame_size,
                            channel.config.max_response_payload_size,
                            ErrorKind::ResponseTooLarge
                        ));

                    // If we made it to this point, we have consumed the frame.
                    if is_new_response {
                        if !channel.outgoing_requests.remove(&header.id()) {
                            return Fatal(header.with_err(ErrorKind::FictitiousRequest));
                        }
                    }

                    match multiframe_outcome {
                        Some(payload) => {
                            // Message is complete.
                            return Success(CompletedRead::ReceivedResponse {
                                id: header.id(),
                                payload: Some(payload.freeze()),
                            });
                        }
                        None => {
                            // We need more frames to complete the payload. Do nothing and attempt
                            // to read the next frame.
                        }
                    }
                }
                Kind::CancelReq => {
                    // Cancellations can be sent multiple times and are not checked to avoid
                    // cancellation races. For security reasons they are subject to an allowance.

                    if channel.cancellation_allowance == 0 {
                        return Fatal(header.with_err(ErrorKind::CancellationLimitExceeded));
                    }
                    channel.cancellation_allowance -= 1;

                    // TODO: What to do with partially received multi-frame request?

                    return Success(CompletedRead::RequestCancellation { id: header.id() });
                }
                Kind::CancelResp => {
                    if channel.outgoing_requests.remove(&header.id()) {
                        return Success(CompletedRead::ResponseCancellation { id: header.id() });
                    } else {
                        return Fatal(header.with_err(ErrorKind::FictitiousCancel));
                    }
                }
            }
        }
    }
}
