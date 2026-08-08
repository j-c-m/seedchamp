//! Upload request validation and block identity (LTEP `reqq` cap).
//!
//! Per-peer piece FIFO lives on the peer-session outbound queue; this module
//! owns geometry checks shared by the peer Request handler.

/// Maximum outstanding upload requests per peer — matches our LTEP `reqq` / pipe cap.
pub const MAX_UPLOAD_REQQ: usize = 8192;

/// Absolute max REQUEST length we accept on the wire (reject above this).
/// Normal peers use 16 KiB blocks; this only bounds abusive sizes.
pub const MAX_REQUEST_LENGTH: u32 = 1 << 17;

/// One queued block the peer asked us to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadBlock {
    pub index: u32,
    pub begin: u32,
    pub length: u32,
}

/// Result of validating an incoming upload Request (B8 / rasterbar-style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadRequestStatus {
    /// Geometry + policy OK — safe to queue / serve.
    Accept,
    /// Invalid or cannot serve — send RejectRequest if Fast, else ignore.
    /// Never disconnect the peer for this alone.
    Reject,
}

/// Validate Request before queue/serve (libtorrent-rasterbar `incoming_request`).
///
/// Checks piece index, begin/length within `piece_len`, max length, have, interested.
pub fn classify_upload_request(
    piece_count: u32,
    piece_len: u32,
    has_piece: bool,
    peer_interested: bool,
    index: u32,
    begin: u32,
    length: u32,
) -> UploadRequestStatus {
    if length == 0 || length > MAX_REQUEST_LENGTH {
        return UploadRequestStatus::Reject;
    }
    if index >= piece_count || piece_len == 0 {
        return UploadRequestStatus::Reject;
    }
    // begin + length must fit this piece (handles last short piece).
    if (begin as u64).saturating_add(length as u64) > piece_len as u64 {
        return UploadRequestStatus::Reject;
    }
    if !peer_interested || !has_piece {
        return UploadRequestStatus::Reject;
    }
    UploadRequestStatus::Accept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_upload_request_geometry() {
        let pc = 10u32;
        let plen = 16384u32 * 2;
        assert_eq!(
            classify_upload_request(pc, plen, true, true, 0, 0, 16384),
            UploadRequestStatus::Accept
        );
        // past piece end
        assert_eq!(
            classify_upload_request(pc, plen, true, true, 0, plen - 100, 200),
            UploadRequestStatus::Reject
        );
        // bad index
        assert_eq!(
            classify_upload_request(pc, plen, true, true, 99, 0, 16384),
            UploadRequestStatus::Reject
        );
        // zero / too long
        assert_eq!(
            classify_upload_request(pc, plen, true, true, 0, 0, 0),
            UploadRequestStatus::Reject
        );
        assert_eq!(
            classify_upload_request(pc, plen, true, true, 0, 0, MAX_REQUEST_LENGTH + 1),
            UploadRequestStatus::Reject
        );
        // not interested / don't have
        assert_eq!(
            classify_upload_request(pc, plen, true, false, 0, 0, 16384),
            UploadRequestStatus::Reject
        );
        assert_eq!(
            classify_upload_request(pc, plen, false, true, 0, 0, 16384),
            UploadRequestStatus::Reject
        );
    }
}
