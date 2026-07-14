//! Bounds-checked little-endian readers shared by the view modules.
//!
//! Every accessor returns `None` (never panics, never wraps) on any out-of-range or overflowing
//! offset; the parsers convert that into their module's error enum — degrade, never panic.

pub(crate) fn u8_at(data: &[u8], off: usize) -> Option<u8> {
    data.get(off).copied()
}

pub(crate) fn u32_le(data: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let bytes: [u8; 4] = data.get(off..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

pub(crate) fn u64_le(data: &[u8], off: usize) -> Option<u64> {
    let end = off.checked_add(8)?;
    let bytes: [u8; 8] = data.get(off..end)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

pub(crate) fn i64_le(data: &[u8], off: usize) -> Option<i64> {
    let end = off.checked_add(8)?;
    let bytes: [u8; 8] = data.get(off..end)?.try_into().ok()?;
    Some(i64::from_le_bytes(bytes))
}

pub(crate) fn array32(data: &[u8], off: usize) -> Option<[u8; 32]> {
    let end = off.checked_add(32)?;
    data.get(off..end)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_bounds_reads() {
        let mut data = [0u8; 64];
        data[0] = 7;
        data[1..9].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        data[9..13].copy_from_slice(&0xAABB_CCDDu32.to_le_bytes());
        data[13..21].copy_from_slice(&(-42i64).to_le_bytes());
        data[21..53].copy_from_slice(&[0x77; 32]);
        assert_eq!(u8_at(&data, 0), Some(7));
        assert_eq!(u64_le(&data, 1), Some(0x0102_0304_0506_0708));
        assert_eq!(u32_le(&data, 9), Some(0xAABB_CCDD));
        assert_eq!(i64_le(&data, 13), Some(-42));
        assert_eq!(array32(&data, 21), Some([0x77; 32]));
    }

    /// Any read touching a byte past the end — or whose `off + width` overflows `usize` — is
    /// `None`, never a panic or a wrap-around read.
    #[test]
    fn out_of_bounds_and_overflow_reads_are_none() {
        let data = [0u8; 16];
        assert_eq!(u8_at(&data, 16), None);
        assert_eq!(u32_le(&data, 13), None);
        assert_eq!(u64_le(&data, 9), None);
        assert_eq!(i64_le(&data, 9), None);
        assert_eq!(array32(&data, 1), None);
        assert_eq!(u32_le(&data, usize::MAX), None);
        assert_eq!(u64_le(&data, usize::MAX - 3), None);
        assert_eq!(array32(&data, usize::MAX - 7), None);
    }
}
