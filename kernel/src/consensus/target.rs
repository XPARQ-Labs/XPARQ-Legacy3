use crypto::{POW_HASH_SIZE, PoWHash};

const COMPACT_MANTISSA_MASK: u32 = 0x007f_ffff;
const COMPACT_SIGN_MASK: u32 = 0x0080_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PoWTarget([u8; POW_HASH_SIZE]);

impl PoWTarget {
    pub fn from_bytes(bytes: [u8; POW_HASH_SIZE]) -> Option<Self> {
        let target = Self(bytes);

        if target.is_zero() { None } else { Some(target) }
    }

    pub const fn as_bytes(&self) -> &[u8; POW_HASH_SIZE] {
        &self.0
    }

    fn is_zero(&self) -> bool {
        self.0.iter().all(|byte| *byte == 0)
    }

    pub fn meets(self, hash: &PoWHash) -> bool {
        hash.as_bytes() <= self.as_bytes()
    }

    pub fn scale_ratio(self, numerator: u32, denominator: u32) -> Option<Self> {
        if numerator == 0
            || denominator == 0
            || numerator > u8::MAX as u32
            || denominator > u8::MAX as u32
        {
            return None;
        }

        let mut wide = [0_u8; POW_HASH_SIZE + 1];
        let mut carry = 0_u32;

        // Multiply the 256-bit big-endian target by numerator.
        for index in (0..POW_HASH_SIZE).rev() {
            let product = u32::from(self.0[index])
                .checked_mul(numerator)?
                .checked_add(carry)?;

            wide[index + 1] = (product & 0xff) as u8;
            carry = product >> 8;
        }

        // Because numerator <= 255, the remaining carry fits in one byte.
        if carry > u8::MAX as u32 {
            return None;
        }

        wide[0] = carry as u8;

        // Divide the 264-bit intermediate by denominator.
        let mut remainder = 0_u32;

        for byte in &mut wide {
            let value = (remainder << 8) | u32::from(*byte);

            *byte = (value / denominator) as u8;
            remainder = value % denominator;
        }

        // Final result must fit back into 256 bits.
        if wide[0] != 0 {
            return None;
        }

        let mut bytes = [0_u8; POW_HASH_SIZE];
        bytes.copy_from_slice(&wide[1..]);

        // Integer division could theoretically reduce a very small target
        // to zero. Consensus must never use a zero target, so saturate at
        // the smallest valid non-zero target.
        if bytes.iter().all(|byte| *byte == 0) {
            bytes[POW_HASH_SIZE - 1] = 1;
        }

        Some(Self(bytes))
    }

    /// Decodes Bitcoin-style compact target representation.
    ///
    /// Layout:
    ///
    /// ```text
    /// [ exponent: 8 bits ][ sign: 1 bit ][ mantissa: 23 bits ]
    /// ```
    ///
    /// XPARQ PoW targets are unsigned, therefore the compact sign bit
    /// is rejected.
    pub fn from_compact(compact: u32) -> Option<Self> {
        let size = (compact >> 24) as usize;
        let mantissa = compact & COMPACT_MANTISSA_MASK;

        // Negative compact targets are invalid.
        if compact & COMPACT_SIGN_MASK != 0 {
            return None;
        }

        if size == 0 || mantissa == 0 {
            return None;
        }

        // Bitcoin-style 256-bit overflow limits.
        let overflow =
            size > 34 || (mantissa > 0xff && size > 33) || (mantissa > 0xffff && size > 32);

        if overflow {
            return None;
        }

        let mut bytes = [0_u8; POW_HASH_SIZE];

        if size <= 3 {
            let shift = 8 * (3 - size);
            let value = mantissa >> shift;

            for index in 0..size {
                let value_shift = 8 * (size - 1 - index);

                bytes[POW_HASH_SIZE - size + index] = ((value >> value_shift) & 0xff) as u8;
            }
        } else {
            let mantissa_bytes = [
                ((mantissa >> 16) & 0xff) as u8,
                ((mantissa >> 8) & 0xff) as u8,
                (mantissa & 0xff) as u8,
            ];

            let base = POW_HASH_SIZE as isize - size as isize;

            for (offset, byte) in mantissa_bytes.into_iter().enumerate() {
                let index = base + offset as isize;

                // Compact targets with exponent 33 or 34 may begin outside
                // the 256-bit buffer. Those bytes must be zero.
                if index < 0 {
                    if byte != 0 {
                        return None;
                    }

                    continue;
                }

                if index >= POW_HASH_SIZE as isize {
                    continue;
                }

                bytes[index as usize] = byte;
            }
        }

        Self::from_bytes(bytes)
    }

    /// Encodes this 256-bit target into Bitcoin-style compact form.
    ///
    /// Compact representation stores only the most significant 23 bits,
    /// therefore arbitrary targets may lose low-order precision.
    pub fn to_compact(self) -> u32 {
        let Some(first_nonzero) = self.0.iter().position(|byte| *byte != 0) else {
            // Normally unreachable because zero targets cannot be
            // constructed through the public constructors.
            return 0;
        };

        let mut size = (POW_HASH_SIZE - first_nonzero) as u32;

        let mut mantissa = if size <= 3 {
            let mut value = 0_u32;

            for byte in &self.0[first_nonzero..] {
                value = (value << 8) | u32::from(*byte);
            }

            value << (8 * (3 - size))
        } else {
            let first = u32::from(self.0[first_nonzero]);

            let second = self
                .0
                .get(first_nonzero + 1)
                .copied()
                .map(u32::from)
                .unwrap_or(0);

            let third = self
                .0
                .get(first_nonzero + 2)
                .copied()
                .map(u32::from)
                .unwrap_or(0);

            (first << 16) | (second << 8) | third
        };

        // Bit 23 is reserved as the compact sign bit.
        //
        // If the mantissa would set it, shift the mantissa right by one
        // byte and increase the exponent.
        if mantissa & COMPACT_SIGN_MASK != 0 {
            mantissa >>= 8;
            size += 1;
        }

        (size << 24) | (mantissa & COMPACT_MANTISSA_MASK)
    }
}

/// Returns true when `hash` satisfies `target`.
#[inline]
pub fn hash_meets_target(hash: &PoWHash, target: PoWTarget) -> bool {
    target.meets(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitcoin_genesis_compact_target_roundtrip() {
        let bits = 0x1d00_ffff;

        let target = PoWTarget::from_compact(bits).expect("valid compact target");

        assert_eq!(target.to_compact(), bits);

        assert_eq!(
            target.as_bytes(),
            &[
                0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00,
            ]
        );
    }

    #[test]
    fn xparq_pow_limit_roundtrip() {
        let bits = 0x207f_ffff;

        let target = PoWTarget::from_compact(bits).expect("valid XPARQ PoW limit");

        assert_eq!(target.to_compact(), bits);
    }

    #[test]
    fn rejects_negative_compact_target() {
        assert!(PoWTarget::from_compact(0x1d80_ffff).is_none());
    }

    #[test]
    fn rejects_zero_compact_target() {
        assert!(PoWTarget::from_compact(0).is_none());
    }

    #[test]
    fn rejects_zero_raw_target() {
        assert!(PoWTarget::from_bytes([0_u8; POW_HASH_SIZE]).is_none());
    }

    #[test]
    fn accepts_smallest_nonzero_raw_target() {
        let mut bytes = [0_u8; POW_HASH_SIZE];
        bytes[POW_HASH_SIZE - 1] = 1;

        assert!(PoWTarget::from_bytes(bytes).is_some());
    }

    #[test]
    fn target_scaling_harder_reduces_target() {
        let target = PoWTarget::from_compact(0x207f_ffff).expect("valid target");

        let harder = target
            .scale_ratio(95, 100)
            .expect("target scaling succeeds");

        assert!(harder < target);
    }

    #[test]
    fn target_scaling_easier_increases_target() {
        let target = PoWTarget::from_compact(0x2007_ffff).expect("valid target");

        let easier = target
            .scale_ratio(105, 100)
            .expect("target scaling succeeds");

        assert!(easier > target);
    }

    #[test]
    fn target_scaling_identity_preserves_target() {
        let target = PoWTarget::from_compact(0x2007_ffff).expect("valid target");

        let same = target
            .scale_ratio(100, 100)
            .expect("target scaling succeeds");

        assert_eq!(same, target);
    }

    #[test]
    fn target_scaling_rejects_large_ratio_values() {
        let target = PoWTarget::from_compact(0x2007_ffff).expect("valid target");

        assert!(target.scale_ratio(256, 100).is_none());
        assert!(target.scale_ratio(100, 256).is_none());
    }

    #[test]
    fn target_scaling_rejects_zero_ratio_values() {
        let target = PoWTarget::from_compact(0x2007_ffff).expect("valid target");

        assert!(target.scale_ratio(0, 100).is_none());
        assert!(target.scale_ratio(100, 0).is_none());
    }

    #[test]
    fn minimum_target_never_scales_to_zero() {
        let mut bytes = [0_u8; POW_HASH_SIZE];
        bytes[POW_HASH_SIZE - 1] = 1;

        let target = PoWTarget::from_bytes(bytes).expect("valid minimum target");

        let harder = target.scale_ratio(95, 100).expect("scaling succeeds");

        assert_eq!(harder, target);
    }

    #[test]
    fn canonical_compact_targets_roundtrip() {
        let targets = [0x1d00_ffff, 0x207f_ffff, 0x2007_ffff, 0x1f12_3456];

        for bits in targets {
            let target = PoWTarget::from_compact(bits).expect("valid compact target");

            assert_eq!(
                target.to_compact(),
                bits,
                "compact target {bits:#010x} did not roundtrip"
            );
        }
    }
}
