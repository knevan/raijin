use thiserror::Error;

use crate::download::{Bytes, DownloadId, DownloadPart, PartId, PartStatus};

/// Errors returned while building persisted range parts.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RangeSplitError {
    /// Requested part count must be greater than zero.
    #[error("part count must be greater than zero")]
    ZeroParts,
    /// Part identifier calculation overflowed.
    #[error("part id overflow for download `{download_id}` part `{index}`")]
    PartIdOverflow { download_id: DownloadId, index: u32 },
}

const PART_ID_STRIDE: i64 = 65_536;

/// Splits a known total byte count into fixed inclusive ranges.
///
/// Unknown or zero-sized resources produce one blind part. Ranged jobs should only
/// use multi-part output when total size is known and greater than zero.
///
/// # Errors
///
/// Returns an error when desired part count is zero or part ids overflow.
pub fn split_fixed_ranges(
    download_id: DownloadId,
    total_bytes: Option<Bytes>,
    desired_parts: u16,
    now_ms: i64,
) -> Result<Vec<DownloadPart>, RangeSplitError> {
    split_fixed_ranges_with_min_part_size(download_id, total_bytes, desired_parts, Bytes::new(1), now_ms)
}

/// Splits a known total byte count into fixed inclusive ranges while respecting a minimum part size.
///
/// Unknown or zero-sized resources produce one blind part. The actual part count is capped by
/// `desired_parts`, total byte count, and `min_part_size` so tiny downloads are not oversplit.
///
/// # Errors
///
/// Returns an error when desired part count is zero or part ids overflow.
pub fn split_fixed_ranges_with_min_part_size(
    download_id: DownloadId,
    total_bytes: Option<Bytes>,
    desired_parts: u16,
    min_part_size: Bytes,
    now_ms: i64,
) -> Result<Vec<DownloadPart>, RangeSplitError> {
    if desired_parts == 0 {
        return Err(RangeSplitError::ZeroParts);
    }

    let Some(total_bytes) = total_bytes else {
        return Ok(vec![part(download_id, 0, 0, None, 0, now_ms)?]);
    };
    let total = total_bytes.get();
    if total == 0 {
        return Ok(vec![part(download_id, 0, 0, None, 0, now_ms)?]);
    }

    let min_part_size = min_part_size.get().max(1);
    let size_limited_parts = total.div_ceil(min_part_size).max(1);
    let part_count = u64::from(desired_parts).min(total).min(size_limited_parts);
    let base_size = total / part_count;
    let remainder = total % part_count;
    let mut start = 0_u64;
    let mut parts = Vec::with_capacity(usize::from(desired_parts));

    for index in 0..part_count {
        let len = base_size + u64::from(index < remainder);
        let end = start + len - 1;
        parts.push(part(
            download_id,
            u32::try_from(index).map_err(|_| RangeSplitError::PartIdOverflow {
                download_id,
                index: u32::MAX,
            })?,
            start,
            Some(end),
            start,
            now_ms,
        )?);
        start = end + 1;
    }

    Ok(parts)
}

fn part(
    download_id: DownloadId,
    index: u32,
    start: u64,
    end: Option<u64>,
    current: u64,
    now_ms: i64,
) -> Result<DownloadPart, RangeSplitError> {
    Ok(DownloadPart {
        id: part_id(download_id, index)?,
        download_id,
        index,
        start_byte: Bytes::new(start),
        end_byte: end.map(Bytes::new),
        current_byte: Bytes::new(current),
        status: PartStatus::Idle,
        retry_count: 0,
        updated_at: now_ms,
    })
}

fn part_id(download_id: DownloadId, index: u32) -> Result<PartId, RangeSplitError> {
    let id = download_id
        .get()
        .checked_mul(PART_ID_STRIDE)
        .and_then(|value| value.checked_add(i64::from(index) + 1))
        .ok_or(RangeSplitError::PartIdOverflow { download_id, index })?;
    Ok(PartId::new(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitter_should_handle_tiny_file() -> Result<(), RangeSplitError> {
        let parts = split_fixed_ranges(DownloadId::new(1), Some(Bytes::new(2)), 4, 1)?;

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].start_byte, Bytes::new(0));
        assert_eq!(parts[0].end_byte, Some(Bytes::new(0)));
        assert_eq!(parts[1].start_byte, Bytes::new(1));
        assert_eq!(parts[1].end_byte, Some(Bytes::new(1)));
        Ok(())
    }

    #[test]
    fn splitter_should_handle_exact_ranges() -> Result<(), RangeSplitError> {
        let parts = split_fixed_ranges(DownloadId::new(1), Some(Bytes::new(100)), 4, 1)?;

        assert_eq!(parts[0].end_byte, Some(Bytes::new(24)));
        assert_eq!(parts[1].start_byte, Bytes::new(25));
        assert_eq!(parts[3].end_byte, Some(Bytes::new(99)));
        Ok(())
    }

    #[test]
    fn splitter_should_handle_uneven_ranges() -> Result<(), RangeSplitError> {
        let parts = split_fixed_ranges(DownloadId::new(1), Some(Bytes::new(10)), 3, 1)?;

        assert_eq!(parts[0].end_byte, Some(Bytes::new(3)));
        assert_eq!(parts[1].start_byte, Bytes::new(4));
        assert_eq!(parts[1].end_byte, Some(Bytes::new(6)));
        assert_eq!(parts[2].start_byte, Bytes::new(7));
        assert_eq!(parts[2].end_byte, Some(Bytes::new(9)));
        Ok(())
    }

    #[test]
    fn splitter_should_handle_unknown_size() -> Result<(), RangeSplitError> {
        let parts = split_fixed_ranges(DownloadId::new(1), None, 4, 1)?;

        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].start_byte, Bytes::ZERO);
        assert_eq!(parts[0].end_byte, None);
        Ok(())
    }

    #[test]
    fn splitter_should_respect_min_part_size() -> Result<(), RangeSplitError> {
        let parts = split_fixed_ranges_with_min_part_size(
            DownloadId::new(1),
            Some(Bytes::new(10)),
            4,
            Bytes::new(4),
            1,
        )?;

        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].end_byte, Some(Bytes::new(3)));
        assert_eq!(parts[2].end_byte, Some(Bytes::new(9)));
        Ok(())
    }

    #[test]
    fn splitter_should_allocate_unique_ids_for_large_part_indexes() -> Result<(), RangeSplitError> {
        let parts = split_fixed_ranges(DownloadId::new(1), Some(Bytes::new(1_024)), 1_024, 1)?;

        assert_eq!(parts[0].id, PartId::new(65_537));
        assert_eq!(parts[1_023].id, PartId::new(66_560));
        Ok(())
    }
}
