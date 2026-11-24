//! QR codes as inline SVG, for the two-factor setup page.

/// The QR code for `text` as an inline `<svg>` element labelled for screen readers, or an
/// empty string when the text will not fit. The renderer's XML prolog is dropped because the
/// page is HTML, and nothing here needs a style attribute or a script, so the CSP holds.
pub fn svg(text: &str) -> String {
    let code = match qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M) {
        Ok(code) => code,
        Err(_) => return String::new(),
    };
    let rendered = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(200, 200)
        .quiet_zone(true)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build();
    let Some(start) = rendered.find("<svg") else { return String::new() };
    rendered[start..].replacen("<svg ", "<svg role=\"img\" aria-label=\"QR code for your authenticator app\" ", 1)
}

#[cfg(test)]
mod tests {
    use super::svg;

    #[test]
    fn qr_is_inline_svg_with_a_label() {
        let s = svg("otpauth://totp/Codehaven:ada?secret=JBSWY3DPEHPK3PXP&issuer=Codehaven");
        assert!(s.starts_with("<svg role=\"img\" aria-label=\"QR code for your authenticator app\" "));
        assert!(s.ends_with("</svg>") || s.trim_end().ends_with("</svg>"));
        assert!(!s.contains("<?xml"));
        assert!(!s.contains("style="));
        assert!(!s.contains("<script"));
    }

    #[test]
    fn qr_of_the_same_text_is_stable() {
        assert_eq!(svg("abc"), svg("abc"));
        assert_ne!(svg("abc"), svg("abd"));
    }

    #[test]
    fn qr_of_text_too_long_is_empty() {
        assert_eq!(svg(&"x".repeat(5000)), "");
    }
}
