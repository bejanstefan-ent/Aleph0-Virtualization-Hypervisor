//! Outcome reporting shared by every VMX instruction.
//!
//! VMX instructions do not raise exceptions on ordinary failure; they report
//! through RFLAGS instead. Each module's error type converts from
//! [`VmxFail`], so callers can use `?` straight after [`check_rflags`].

/// How a VMX instruction failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxFail {
    /// CF=1: failed with no current VMCS to record why (VMfailInvalid).
    Invalid,
    /// ZF=1: failed and stored a reason in the current VMCS's
    /// VM-instruction error field (VMfailValid).
    Valid,
}

/// Decodes the RFLAGS value a VMX instruction leaves behind.
///
/// All flags clear on success, CF set for VMfailInvalid, ZF set for
/// VMfailValid. Call this right after the `asm!` block instead of repeating
/// the bit tests.
pub fn check_rflags(rflags: u64) -> Result<(), VmxFail> {
    /// RFLAGS.CF, bit 0.
    const CF: u64 = 1 << 0;
    /// RFLAGS.ZF, bit 6.
    const ZF: u64 = 1 << 6;

    if rflags & CF != 0 {
        Err(VmxFail::Invalid)
    } else if rflags & ZF != 0 {
        Err(VmxFail::Valid)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CF: u64 = 1 << 0;
    const ZF: u64 = 1 << 6;
    /// RFLAGS bit 1 always reads as 1.
    const RESERVED: u64 = 1 << 1;

    #[test]
    fn clear_flags_are_success() {
        assert_eq!(check_rflags(RESERVED), Ok(()));
        // IF and other unrelated flags do not change the outcome.
        assert_eq!(check_rflags(RESERVED | 1 << 9 | 1 << 2), Ok(()));
    }

    #[test]
    fn cf_is_fail_invalid() {
        assert_eq!(check_rflags(RESERVED | CF), Err(VmxFail::Invalid));
    }

    #[test]
    fn zf_is_fail_valid() {
        assert_eq!(check_rflags(RESERVED | ZF), Err(VmxFail::Valid));
    }

    #[test]
    fn cf_wins_over_zf() {
        assert_eq!(check_rflags(CF | ZF), Err(VmxFail::Invalid));
    }
}
