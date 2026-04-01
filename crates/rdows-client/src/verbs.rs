use bytes::Bytes;

use rdows_core::error::{ErrorCode, RdowsError};
use rdows_core::memory::{AccessFlags, LKey, RKey};
use rdows_core::message::{
    AtomicReqPayload, DataPayload, MrDeregPayload, MrRegPayload, ReadReqPayload, RdowsMessage,
    SendPayload, WritePayload, ATOMIC_TYPE_CAS, ATOMIC_TYPE_FAA,
};
use rdows_core::opcode::Opcode;
use rdows_core::queue::{CompletionQueueEntry, ScatterGatherEntry, WorkRequestId};

use crate::connection;
use crate::RdowsConnection;

pub struct MemoryRegionHandle {
    pub lkey: LKey,
    pub rkey: RKey,
    pub len: u64,
    pub local_buf: Vec<u8>,
}

impl RdowsConnection {
    // -----------------------------------------------------------------------
    // Memory Region management
    // -----------------------------------------------------------------------

    pub async fn reg_mr(
        &mut self,
        access_flags: AccessFlags,
        region_len: u64,
    ) -> Result<MemoryRegionHandle, RdowsError> {
        let header = self.next_header(Opcode::MrReg, 0);
        let payload = MrRegPayload {
            pd: self.pd,
            access_flags,
            region_len,
            suggested_lkey: None,
        };
        let msg = RdowsMessage::MrReg(header, payload);
        let resp = self.send_and_recv(msg).await?;

        match resp {
            RdowsMessage::MrRegAck(_, ack) => {
                if ack.status != 0 {
                    return Err(RdowsError::Protocol(
                        rdows_core::error::ErrorCode::try_from(ack.status)
                            .unwrap_or(rdows_core::error::ErrorCode::ErrInternal),
                    ));
                }
                self.local_mrs.insert(
                    ack.lkey.0,
                    MemoryRegionHandle {
                        lkey: ack.lkey,
                        rkey: ack.rkey,
                        len: region_len,
                        local_buf: vec![0u8; region_len as usize],
                    },
                );
                Ok(MemoryRegionHandle {
                    lkey: ack.lkey,
                    rkey: ack.rkey,
                    len: region_len,
                    local_buf: vec![0u8; region_len as usize],
                })
            }
            RdowsMessage::Error(_, err) => Err(RdowsError::Protocol(err.error_code)),
            other => Err(RdowsError::UnexpectedMessage {
                expected: "MR_REG_ACK",
                got: other.header().opcode,
            }),
        }
    }

    pub async fn dereg_mr(&mut self, lkey: LKey) -> Result<(), RdowsError> {
        let header = self.next_header(Opcode::MrDereg, 0);
        let payload = MrDeregPayload {
            pd: self.pd,
            lkey,
        };
        let msg = RdowsMessage::MrDereg(header, payload);
        let resp = self.send_and_recv(msg).await?;

        match resp {
            RdowsMessage::MrDeregAck(_, ack) => {
                if ack.status != 0 {
                    return Err(RdowsError::Protocol(
                        rdows_core::error::ErrorCode::try_from(ack.status)
                            .unwrap_or(rdows_core::error::ErrorCode::ErrInternal),
                    ));
                }
                self.local_mrs.remove(&lkey.0);
                Ok(())
            }
            RdowsMessage::Error(_, err) => Err(RdowsError::Protocol(err.error_code)),
            other => Err(RdowsError::UnexpectedMessage {
                expected: "MR_DEREG_ACK",
                got: other.header().opcode,
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Two-sided: SEND/RECV
    // -----------------------------------------------------------------------

    pub async fn post_send(
        &mut self,
        wrid: u64,
        sg_list: &[ScatterGatherEntry],
    ) -> Result<(), RdowsError> {
        if self.cq.is_full() {
            return Err(RdowsError::Protocol(ErrorCode::ErrCqOverflow));
        }
        if self.send_credits == 0 {
            return Err(RdowsError::SendCreditsExhausted);
        }
        // Gather data from local MRs
        let data = self.gather_sg_data(sg_list)?;
        let total_len = data.len() as u32;

        // Send SEND message with SG list
        let header = self.next_header(Opcode::Send, wrid);
        let send_payload = SendPayload {
            sg_list: sg_list.to_vec(),
        };
        connection::send_message(&mut self.sink, &RdowsMessage::Send(header, send_payload))
            .await?;

        // Send SEND_DATA with gathered bytes
        let data_header = self.next_header(Opcode::SendData, wrid);
        let data_payload = DataPayload {
            data: Bytes::from(data),
        };
        connection::send_message(
            &mut self.sink,
            &RdowsMessage::SendData(data_header, data_payload),
        )
        .await?;

        // Await RECV_COMP
        let resp = self.recv_app_message().await?;
        match resp {
            RdowsMessage::RecvComp(_) => {
                self.send_credits -= 1;
                self.cq.push(CompletionQueueEntry {
                    wrid: WorkRequestId(wrid),
                    status: 0,
                    opcode: Opcode::Send,
                    vendor_error: 0,
                    byte_count: total_len,
                    qp_number: self.session_id,
                });
                Ok(())
            }
            RdowsMessage::Error(_, err) => {
                self.cq.push(CompletionQueueEntry {
                    wrid: WorkRequestId(wrid),
                    status: err.error_code.into(),
                    opcode: Opcode::Send,
                    vendor_error: 0,
                    byte_count: 0,
                    qp_number: self.session_id,
                });
                Err(RdowsError::Protocol(err.error_code))
            }
            other => Err(RdowsError::UnexpectedMessage {
                expected: "RECV_COMP",
                got: other.header().opcode,
            }),
        }
    }

    // -----------------------------------------------------------------------
    // One-sided: RDMA Write
    // -----------------------------------------------------------------------

    pub async fn rdma_write(
        &mut self,
        wrid: u64,
        rkey: RKey,
        remote_va: u64,
        sg_list: &[ScatterGatherEntry],
    ) -> Result<(), RdowsError> {
        if self.cq.is_full() {
            return Err(RdowsError::Protocol(ErrorCode::ErrCqOverflow));
        }
        let data = self.gather_sg_data(sg_list)?;
        let total_len = data.len() as u64;

        // Send WRITE message
        let header = self.next_header(Opcode::Write, wrid);
        let write_payload = WritePayload {
            rkey,
            remote_va,
            length: total_len,
            sg_list: sg_list.to_vec(),
        };
        connection::send_message(
            &mut self.sink,
            &RdowsMessage::Write(header, write_payload),
        )
        .await?;

        // Send WRITE_DATA
        let data_header = self.next_header(Opcode::WriteData, wrid);
        let data_payload = DataPayload {
            data: Bytes::from(data),
        };
        connection::send_message(
            &mut self.sink,
            &RdowsMessage::WriteData(data_header, data_payload),
        )
        .await?;

        // Await WRITE_COMP
        let resp = self.recv_app_message().await?;
        match resp {
            RdowsMessage::WriteComp(_) => {
                self.cq.push(CompletionQueueEntry {
                    wrid: WorkRequestId(wrid),
                    status: 0,
                    opcode: Opcode::Write,
                    vendor_error: 0,
                    byte_count: total_len as u32,
                    qp_number: self.session_id,
                });
                Ok(())
            }
            RdowsMessage::Error(_, err) => {
                self.cq.push(CompletionQueueEntry {
                    wrid: WorkRequestId(wrid),
                    status: err.error_code.into(),
                    opcode: Opcode::Write,
                    vendor_error: 0,
                    byte_count: 0,
                    qp_number: self.session_id,
                });
                Err(RdowsError::Protocol(err.error_code))
            }
            other => Err(RdowsError::UnexpectedMessage {
                expected: "WRITE_COMP",
                got: other.header().opcode,
            }),
        }
    }

    // -----------------------------------------------------------------------
    // One-sided: RDMA Read
    // -----------------------------------------------------------------------

    pub async fn rdma_read(
        &mut self,
        wrid: u64,
        rkey: RKey,
        remote_va: u64,
        read_len: u64,
        local_lkey: LKey,
        local_va: u64,
    ) -> Result<(), RdowsError> {
        if self.cq.is_full() {
            return Err(RdowsError::Protocol(ErrorCode::ErrCqOverflow));
        }
        let header = self.next_header(Opcode::ReadReq, wrid);
        let payload = ReadReqPayload {
            rkey,
            remote_va,
            read_len,
            local_lkey,
            local_va,
        };
        connection::send_message(
            &mut self.sink,
            &RdowsMessage::ReadReq(header, payload),
        )
        .await?;

        // Await READ_RESP
        let resp = self.recv_app_message().await?;
        match resp {
            RdowsMessage::ReadResp(_, resp_payload) => {
                // Copy data into local MR
                let mr = self
                    .local_mrs
                    .get_mut(&local_lkey.0)
                    .ok_or(RdowsError::Protocol(
                        rdows_core::error::ErrorCode::ErrInvalidLkey,
                    ))?;

                let start = local_va as usize;
                let end = start + resp_payload.data.len();
                if end > mr.local_buf.len() {
                    return Err(RdowsError::Protocol(
                        rdows_core::error::ErrorCode::ErrBounds,
                    ));
                }
                mr.local_buf[start..end].copy_from_slice(&resp_payload.data);

                self.cq.push(CompletionQueueEntry {
                    wrid: WorkRequestId(wrid),
                    status: 0,
                    opcode: Opcode::ReadReq,
                    vendor_error: 0,
                    byte_count: resp_payload.data.len() as u32,
                    qp_number: self.session_id,
                });
                Ok(())
            }
            RdowsMessage::Error(_, err) => {
                self.cq.push(CompletionQueueEntry {
                    wrid: WorkRequestId(wrid),
                    status: err.error_code.into(),
                    opcode: Opcode::ReadReq,
                    vendor_error: 0,
                    byte_count: 0,
                    qp_number: self.session_id,
                });
                Err(RdowsError::Protocol(err.error_code))
            }
            other => Err(RdowsError::UnexpectedMessage {
                expected: "READ_RESP",
                got: other.header().opcode,
            }),
        }
    }

    // -----------------------------------------------------------------------
    // One-sided: Atomic CAS / FAA
    // -----------------------------------------------------------------------

    pub async fn atomic_cas(
        &mut self,
        wrid: u64,
        rkey: RKey,
        remote_va: u64,
        compare: u64,
        swap: u64,
    ) -> Result<u64, RdowsError> {
        self.post_atomic(wrid, rkey, remote_va, ATOMIC_TYPE_CAS, compare, swap)
            .await
    }

    pub async fn atomic_faa(
        &mut self,
        wrid: u64,
        rkey: RKey,
        remote_va: u64,
        addend: u64,
    ) -> Result<u64, RdowsError> {
        self.post_atomic(wrid, rkey, remote_va, ATOMIC_TYPE_FAA, addend, 0)
            .await
    }

    async fn post_atomic(
        &mut self,
        wrid: u64,
        rkey: RKey,
        remote_va: u64,
        atomic_type: u8,
        operand1: u64,
        operand2: u64,
    ) -> Result<u64, RdowsError> {
        if self.cq.is_full() {
            return Err(RdowsError::Protocol(ErrorCode::ErrCqOverflow));
        }
        let header = self.next_header(Opcode::AtomicReq, wrid);
        let payload = AtomicReqPayload {
            rkey,
            atomic_type,
            remote_va,
            operand1,
            operand2,
        };
        connection::send_message(
            &mut self.sink,
            &RdowsMessage::AtomicReq(header, payload),
        )
        .await?;

        let resp = self.recv_app_message().await?;
        match resp {
            RdowsMessage::AtomicResp(_, resp_payload) => {
                self.cq.push(CompletionQueueEntry {
                    wrid: WorkRequestId(wrid),
                    status: 0,
                    opcode: Opcode::AtomicReq,
                    vendor_error: 0,
                    byte_count: 8,
                    qp_number: self.session_id,
                });
                Ok(resp_payload.original_value)
            }
            RdowsMessage::Error(_, err) => {
                self.cq.push(CompletionQueueEntry {
                    wrid: WorkRequestId(wrid),
                    status: err.error_code.into(),
                    opcode: Opcode::AtomicReq,
                    vendor_error: 0,
                    byte_count: 0,
                    qp_number: self.session_id,
                });
                Err(RdowsError::Protocol(err.error_code))
            }
            other => Err(RdowsError::UnexpectedMessage {
                expected: "ATOMIC_RESP",
                got: other.header().opcode,
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn gather_sg_data(&self, sg_list: &[ScatterGatherEntry]) -> Result<Vec<u8>, RdowsError> {
        let mut result = Vec::new();
        for sg in sg_list {
            let mr = self
                .local_mrs
                .get(&sg.lkey.0)
                .ok_or(RdowsError::Protocol(
                    rdows_core::error::ErrorCode::ErrInvalidLkey,
                ))?;
            let start = sg.offset as usize;
            let end = start + sg.length as usize;
            if end > mr.local_buf.len() {
                return Err(RdowsError::Protocol(
                    rdows_core::error::ErrorCode::ErrBounds,
                ));
            }
            result.extend_from_slice(&mr.local_buf[start..end]);
        }
        Ok(result)
    }

    /// Write data directly into a local memory region's buffer.
    pub fn write_local_mr(
        &mut self,
        lkey: LKey,
        offset: usize,
        data: &[u8],
    ) -> Result<(), RdowsError> {
        let mr = self
            .local_mrs
            .get_mut(&lkey.0)
            .ok_or(RdowsError::Protocol(
                rdows_core::error::ErrorCode::ErrInvalidLkey,
            ))?;
        let end = offset + data.len();
        if end > mr.local_buf.len() {
            return Err(RdowsError::Protocol(
                rdows_core::error::ErrorCode::ErrBounds,
            ));
        }
        mr.local_buf[offset..end].copy_from_slice(data);
        Ok(())
    }

    /// Read data from a local memory region's buffer.
    pub fn read_local_mr(
        &self,
        lkey: LKey,
        offset: usize,
        len: usize,
    ) -> Result<Vec<u8>, RdowsError> {
        let mr = self
            .local_mrs
            .get(&lkey.0)
            .ok_or(RdowsError::Protocol(
                rdows_core::error::ErrorCode::ErrInvalidLkey,
            ))?;
        let end = offset + len;
        if end > mr.local_buf.len() {
            return Err(RdowsError::Protocol(
                rdows_core::error::ErrorCode::ErrBounds,
            ));
        }
        Ok(mr.local_buf[offset..end].to_vec())
    }
}
