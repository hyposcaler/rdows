/// Wire-level error codes per RFC Section 11.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ErrorCode {
    Success = 0x0000,
    ErrProtoVersion = 0x0001,
    ErrUnknownOpcode = 0x0002,
    ErrInvalidPd = 0x0003,
    ErrInvalidLkey = 0x0004,
    ErrInvalidMkey = 0x0005,
    ErrAccessDenied = 0x0006,
    ErrBounds = 0x0007,
    ErrAlignment = 0x0008,
    ErrPayloadSize = 0x0009,
    ErrRnr = 0x0010,
    ErrCqOverflow = 0x0020,
    ErrSeqGap = 0x0030,
    ErrTimeout = 0x0040,
    ErrInternal = 0xFFFF,
}

impl From<ErrorCode> for u16 {
    fn from(code: ErrorCode) -> u16 {
        code as u16
    }
}

impl TryFrom<u16> for ErrorCode {
    type Error = InvalidErrorCode;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0x0000 => Ok(ErrorCode::Success),
            0x0001 => Ok(ErrorCode::ErrProtoVersion),
            0x0002 => Ok(ErrorCode::ErrUnknownOpcode),
            0x0003 => Ok(ErrorCode::ErrInvalidPd),
            0x0004 => Ok(ErrorCode::ErrInvalidLkey),
            0x0005 => Ok(ErrorCode::ErrInvalidMkey),
            0x0006 => Ok(ErrorCode::ErrAccessDenied),
            0x0007 => Ok(ErrorCode::ErrBounds),
            0x0008 => Ok(ErrorCode::ErrAlignment),
            0x0009 => Ok(ErrorCode::ErrPayloadSize),
            0x0010 => Ok(ErrorCode::ErrRnr),
            0x0020 => Ok(ErrorCode::ErrCqOverflow),
            0x0030 => Ok(ErrorCode::ErrSeqGap),
            0x0040 => Ok(ErrorCode::ErrTimeout),
            0xFFFF => Ok(ErrorCode::ErrInternal),
            _ => Err(InvalidErrorCode(value)),
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorCode::Success => write!(f, "SUCCESS"),
            ErrorCode::ErrProtoVersion => write!(f, "ERR_PROTO_VERSION"),
            ErrorCode::ErrUnknownOpcode => write!(f, "ERR_UNKNOWN_OPCODE"),
            ErrorCode::ErrInvalidPd => write!(f, "ERR_INVALID_PD"),
            ErrorCode::ErrInvalidLkey => write!(f, "ERR_INVALID_LKEY"),
            ErrorCode::ErrInvalidMkey => write!(f, "ERR_INVALID_MKEY"),
            ErrorCode::ErrAccessDenied => write!(f, "ERR_ACCESS_DENIED"),
            ErrorCode::ErrBounds => write!(f, "ERR_BOUNDS"),
            ErrorCode::ErrAlignment => write!(f, "ERR_ALIGNMENT"),
            ErrorCode::ErrPayloadSize => write!(f, "ERR_PAYLOAD_SIZE"),
            ErrorCode::ErrRnr => write!(f, "ERR_RNR"),
            ErrorCode::ErrCqOverflow => write!(f, "ERR_CQ_OVERFLOW"),
            ErrorCode::ErrSeqGap => write!(f, "ERR_SEQ_GAP"),
            ErrorCode::ErrTimeout => write!(f, "ERR_TIMEOUT"),
            ErrorCode::ErrInternal => write!(f, "ERR_INTERNAL"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidErrorCode(pub u16);

impl std::fmt::Display for InvalidErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid RDoWS error code: 0x{:04X}", self.0)
    }
}

impl std::error::Error for InvalidErrorCode {}

/// Library-level errors for RDoWS operations.
#[derive(Debug, thiserror::Error)]
pub enum RdowsError {
    #[error("invalid protocol version: 0x{0:02X}")]
    InvalidVersion(u8),

    #[error("invalid opcode: 0x{0:02X}")]
    InvalidOpcode(u8),

    #[error("payload too short: expected at least {expected} bytes, got {got}")]
    PayloadTooShort { expected: usize, got: usize },

    #[error("header too short: need 24 bytes, got {0}")]
    HeaderTooShort(usize),

    #[error("protocol error: {0}")]
    Protocol(ErrorCode),

    #[error("connection rejected: {0}")]
    ConnectionRejected(String),

    #[error("session not ready")]
    SessionNotReady,

    #[error("session closed")]
    SessionClosed,

    #[error("send credits exhausted")]
    SendCreditsExhausted,

    #[error("unexpected message: expected {expected}, got {got:?}")]
    UnexpectedMessage {
        expected: &'static str,
        got: crate::opcode::Opcode,
    },

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("tls error: {0}")]
    Tls(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_error_codes() {
        let codes = [
            (0x0000u16, ErrorCode::Success),
            (0x0001, ErrorCode::ErrProtoVersion),
            (0x0002, ErrorCode::ErrUnknownOpcode),
            (0x0003, ErrorCode::ErrInvalidPd),
            (0x0004, ErrorCode::ErrInvalidLkey),
            (0x0005, ErrorCode::ErrInvalidMkey),
            (0x0006, ErrorCode::ErrAccessDenied),
            (0x0007, ErrorCode::ErrBounds),
            (0x0008, ErrorCode::ErrAlignment),
            (0x0009, ErrorCode::ErrPayloadSize),
            (0x0010, ErrorCode::ErrRnr),
            (0x0020, ErrorCode::ErrCqOverflow),
            (0x0030, ErrorCode::ErrSeqGap),
            (0x0040, ErrorCode::ErrTimeout),
            (0xFFFF, ErrorCode::ErrInternal),
        ];

        for (val, expected) in codes {
            let code = ErrorCode::try_from(val).unwrap();
            assert_eq!(code, expected);
            assert_eq!(u16::from(code), val);
        }
    }

    #[test]
    fn unknown_error_code() {
        assert!(ErrorCode::try_from(0x00FF).is_err());
        assert!(ErrorCode::try_from(0x000A).is_err());
    }
}
