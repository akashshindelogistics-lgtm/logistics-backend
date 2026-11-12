//! Input rules for the GST / e-way bill fields stored on orgs, customers,
//! stock categories and dispatches. See `docs/gst.md`.

/// Normalize an optional GSTIN: trim it, treat blank as `None`, upper-case it.
/// It must be 15 letters/digits — the same rule vendors use; the GSTIN
/// checksum is deliberately not verified.
pub fn normalize_gstin(value: Option<String>) -> Result<Option<String>, String> {
    let Some(g) = value
        .map(|v| v.trim().to_ascii_uppercase())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    if g.len() != 15 || !g.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("GSTIN must be 15 letters and digits".into());
    }
    Ok(Some(g))
}

/// Normalize an optional e-way bill number: trim it, treat blank as `None`.
/// It must be exactly 12 digits.
pub fn normalize_eway_bill_no(value: Option<String>) -> Result<Option<String>, String> {
    let Some(n) = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    if n.len() != 12 || !n.chars().all(|c| c.is_ascii_digit()) {
        return Err("e-way bill number must be 12 digits".into());
    }
    Ok(Some(n))
}

/// Normalize an optional HSN code: trim it, treat blank as `None`. HSN codes
/// are 2 to 8 digits.
pub fn normalize_hsn_code(value: Option<String>) -> Result<Option<String>, String> {
    let Some(h) = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    if !(2..=8).contains(&h.len()) || !h.chars().all(|c| c.is_ascii_digit()) {
        return Err("HSN code must be 2 to 8 digits".into());
    }
    Ok(Some(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gstin_is_trimmed_upper_cased_and_checked() {
        assert_eq!(
            normalize_gstin(Some(" 27aapfu0939f1zv ".into())).unwrap(),
            Some("27AAPFU0939F1ZV".into())
        );
        assert_eq!(normalize_gstin(Some("  ".into())).unwrap(), None);
        assert_eq!(normalize_gstin(None).unwrap(), None);
        assert!(normalize_gstin(Some("SHORT".into())).is_err());
        assert!(normalize_gstin(Some("27AAPFU0939F1Z-".into())).is_err());
    }

    #[test]
    fn eway_bill_must_be_twelve_digits() {
        assert_eq!(
            normalize_eway_bill_no(Some(" 123456789012 ".into())).unwrap(),
            Some("123456789012".into())
        );
        assert_eq!(normalize_eway_bill_no(Some("".into())).unwrap(), None);
        assert!(normalize_eway_bill_no(Some("12345678901".into())).is_err());
        assert!(normalize_eway_bill_no(Some("12345678901A".into())).is_err());
    }

    #[test]
    fn hsn_must_be_two_to_eight_digits() {
        assert_eq!(
            normalize_hsn_code(Some("2523".into())).unwrap(),
            Some("2523".into())
        );
        assert_eq!(normalize_hsn_code(None).unwrap(), None);
        assert!(normalize_hsn_code(Some("1".into())).is_err());
        assert!(normalize_hsn_code(Some("123456789".into())).is_err());
        assert!(normalize_hsn_code(Some("25A3".into())).is_err());
    }
}
