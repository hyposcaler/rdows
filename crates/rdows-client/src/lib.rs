pub mod completion;
pub mod connection;
pub mod verbs;

use rdows_core::error::{ErrorCode, RdowsError};
use rdows_core::frame::RdowsHeader;
use rdows_core::memory::ProtectionDomain;
use rdows_core::message::RdowsMessage;
use rdows_core::opcode::Opcode;

use crate::completion::CompletionQueue;
use crate::connection::{ClientSink, ClientStream, ConnectionParams};

pub use rdows_core;

const DEFAULT_CQ_CAPACITY: usize = 65536;

pub struct ConnectConfig {
    pub cq_capacity: usize,
}

impl Default for ConnectConfig {
    fn default() -> Self {
        Self {
            cq_capacity: DEFAULT_CQ_CAPACITY,
        }
    }
}

pub struct RdowsConnection {
    pub(crate) sink: ClientSink,
    pub(crate) stream: ClientStream,
    pub(crate) session_id: u32,
    pub(crate) pd: ProtectionDomain,
    #[allow(dead_code)]
    pub(crate) max_msg_size: u32,
    pub(crate) next_seq: u32,
    pub(crate) expected_remote_seq: Option<u32>,
    pub(crate) send_credits: u32,
    pub(crate) cq: CompletionQueue,
    pub(crate) local_mrs: std::collections::HashMap<u32, verbs::MemoryRegionHandle>,
}

impl RdowsConnection {
    pub async fn connect(
        url: &str,
        tls_config: rustls::ClientConfig,
    ) -> Result<Self, RdowsError> {
        Self::connect_with_config(url, tls_config, ConnectConfig::default()).await
    }

    pub async fn connect_with_config(
        url: &str,
        tls_config: rustls::ClientConfig,
        config: ConnectConfig,
    ) -> Result<Self, RdowsError> {
        let (sink, stream, params) = connection::connect(url, tls_config).await?;
        let ConnectionParams {
            session_id,
            pd,
            max_msg_size,
            initial_remote_seq,
            icc,
        } = params;

        Ok(Self {
            sink,
            stream,
            session_id,
            pd,
            max_msg_size,
            next_seq: 1, // 0 was used for CONNECT
            expected_remote_seq: Some(initial_remote_seq),
            send_credits: icc,
            cq: CompletionQueue::new(config.cq_capacity),
            local_mrs: std::collections::HashMap::new(),
        })
    }

    pub async fn disconnect(mut self) -> Result<(), RdowsError> {
        let header = self.next_header(Opcode::Disconnect, 0);
        let msg = RdowsMessage::Disconnect(header);
        connection::send_message(&mut self.sink, &msg).await
    }

    pub fn poll_cq(&mut self, max: usize) -> Vec<rdows_core::queue::CompletionQueueEntry> {
        self.cq.poll_cq(max)
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    pub(crate) fn next_header(&mut self, opcode: Opcode, wrid: u64) -> RdowsHeader {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        RdowsHeader::new(opcode, self.session_id, seq, wrid)
    }

    pub fn set_send_credits(&mut self, credits: u32) {
        self.send_credits = credits;
    }

    pub(crate) fn validate_remote_seq(&mut self, msg: &RdowsMessage) -> Result<(), RdowsError> {
        if let Some(expected) = self.expected_remote_seq {
            if msg.header().sequence != expected {
                return Err(RdowsError::Protocol(ErrorCode::ErrSeqGap));
            }
        }
        self.expected_remote_seq = Some(msg.header().sequence.wrapping_add(1));
        Ok(())
    }

    pub(crate) async fn recv_app_message(&mut self) -> Result<RdowsMessage, RdowsError> {
        loop {
            let resp = connection::recv_message(&mut self.stream).await?;
            self.validate_remote_seq(&resp)?;
            match resp {
                RdowsMessage::CreditUpdate(_, payload) => {
                    self.send_credits += payload.credit_increment;
                    continue;
                }
                other => return Ok(other),
            }
        }
    }

    pub(crate) async fn send_and_recv(
        &mut self,
        msg: RdowsMessage,
    ) -> Result<RdowsMessage, RdowsError> {
        connection::send_message(&mut self.sink, &msg).await?;
        self.recv_app_message().await
    }
}
