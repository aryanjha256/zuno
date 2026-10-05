//! A server's TLS certificate, read into the few fields someone debugging a connection looks at.
//!
//! reqwest's `TlsInfo` gives the leaf certificate as DER bytes and nothing else — no protocol
//! version, no cipher — so this is everything the Network tab can say about TLS without
//! replacing reqwest's connector. Read on the engine thread, never the UI one (invariant 3).

use sha1::{Digest, Sha1};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateInfo {
    /// The subject's common name, `*.example.com`, or empty when it has none — modern
    /// certificates may name hosts only in `alt_names`.
    pub subject: String,
    /// Subject alternative names that are DNS names, which is what a hostname is matched against.
    pub alt_names: Vec<String>,
    /// Organisation and common name, `Google Trust Services · WR3`.
    pub issuer: String,
    pub not_before: String,
    pub not_after: String,
    /// `not_after` as Unix seconds, so a caller can say "expired" or "expires in 3 days".
    pub not_after_unix: i64,
    /// Colon-separated hex, as browsers print it.
    pub serial: String,
    /// SHA-1 of the DER, colon-separated — the fingerprint Postman shows, so the two compare.
    pub sha1_fingerprint: String,
}

/// Read a DER certificate. `None` for bytes that are not one — the view then says nothing about
/// the certificate rather than showing a half-parsed one.
pub fn read(der: &[u8]) -> Option<CertificateInfo> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;

    let first = |iter: &mut dyn Iterator<Item = &x509_parser::x509::AttributeTypeAndValue<'_>>| {
        iter.next()
            .and_then(|value| value.as_str().ok())
            .map(str::to_string)
    };

    let subject = first(&mut cert.subject().iter_common_name()).unwrap_or_default();
    let issuer = [
        first(&mut cert.issuer().iter_organization()),
        first(&mut cert.issuer().iter_common_name()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");

    let alt_names = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|extension| {
            extension
                .value
                .general_names
                .iter()
                .filter_map(|name| match name {
                    x509_parser::extensions::GeneralName::DNSName(dns) => Some(dns.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    let validity = cert.validity();
    Some(CertificateInfo {
        subject,
        alt_names,
        issuer,
        not_before: validity.not_before.to_string(),
        not_after: validity.not_after.to_string(),
        not_after_unix: validity.not_after.timestamp(),
        serial: cert.raw_serial_as_string(),
        sha1_fingerprint: colon_hex(&Sha1::digest(der)),
    })
}

fn colon_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned against `openssl x509 -noout -subject -issuer -serial -fingerprint -sha1` on the
    /// same fixture, so these are the values a second tool reads, not values this code agrees
    /// with itself about.
    #[test]
    fn a_certificate_reads_as_openssl_reads_it() {
        let pem = include_bytes!("../tests/fixtures/root-ca.pem");
        let (_, pem) = x509_parser::pem::parse_x509_pem(pem).expect("pem");
        let cert = read(&pem.contents).expect("a certificate");

        assert_eq!(cert.subject, "zuno-test");
        assert_eq!(cert.issuer, "zuno-test", "no organisation, so the name alone");
        assert_eq!(
            cert.sha1_fingerprint,
            "37:C7:19:BB:64:3F:EE:92:A1:32:D0:1F:B9:AD:AA:74:FA:D8:72:4A"
        );
        assert_eq!(
            cert.serial.replace(':', "").to_uppercase(),
            "74EA40790189C993E1D05B4878AFFA795F090324"
        );
        assert!(cert.not_after.contains("2036"), "{}", cert.not_after);
    }

    #[test]
    fn bytes_that_are_not_a_certificate_read_as_none() {
        assert_eq!(read(b"not a certificate"), None);
        assert_eq!(read(&[]), None);
    }

    #[test]
    fn a_fingerprint_is_colon_separated_upper_hex() {
        assert_eq!(colon_hex(&[0x0d, 0x1e, 0xff]), "0D:1E:FF");
    }
}
