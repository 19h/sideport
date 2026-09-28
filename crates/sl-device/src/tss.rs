//! Personalized DDI signing through Apple's TSS controller (recovered `fetchManifestFromTSS`).
//!
//! The request carries the device's `ApBoardID`/`ApChipID`/`ApECID`/`ApNonce`/`SepNonce` and the
//! DDI `BuildManifest` rules; Apple returns a signed `ApImg4Ticket` used as the personalized
//! image signature. The endpoint is configurable so tests use wiremock and never contact Apple.
//!
//! The manifest math (build-identity selection, `RestoreRequestRules`) reuses `idevice`'s tss
//! helpers; the request keys and response parsing follow the recovered Go.

use crate::error::{DeviceError, Result};
use plist::{Dictionary, Value};

/// Recovered TSS controller endpoint.
pub const DEFAULT_ENDPOINT: &str = "http://gs.apple.com/TSS/controller?action=2";

/// Recovered client version string (`@VersionInfo`).
const VERSION_INFO: &str = "libauthinstall-973.0.1";

/// A TSS controller, its endpoint overridable for tests.
#[derive(Debug, Clone)]
pub struct TssClient {
    endpoint: String,
}

impl Default for TssClient {
    fn default() -> Self {
        Self { endpoint: DEFAULT_ENDPOINT.into() }
    }
}

impl TssClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self { endpoint: endpoint.into() }
    }

    /// Build the personalization request, send it, and return the signed `ApImg4Ticket`.
    pub async fn personalization_manifest(
        &self,
        identifiers: &Dictionary,
        ap_nonce: Vec<u8>,
        build_manifest: &[u8],
    ) -> Result<Vec<u8>> {
        let manifest: Dictionary =
            plist::from_bytes(build_manifest).map_err(|error| DeviceError::Protocol(error.to_string()))?;

        let request = build_request(identifiers, ap_nonce, &manifest)?;

        let response = self.send(&request).await?;

        idevice::tss::extract_img4_ticket(&response).map_err(DeviceError::from)
    }

    async fn send(&self, request: &Dictionary) -> Result<Dictionary> {
        let mut body = Vec::new();
        Value::Dictionary(request.clone())
            .to_writer_xml(&mut body)
            .map_err(|error| DeviceError::Protocol(error.to_string()))?;

        let client = reqwest::Client::new();
        let response = client
            .post(&self.endpoint)
            .header("Cache-Control", "no-cache")
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("User-Agent", "InetURL/1.0")
            .body(body)
            .send()
            .await
            .map_err(|error| DeviceError::Remote(format!("TSS request failed: {error}")))?;

        let status = response.status();
        let text = response.text().await.map_err(|error| DeviceError::Remote(error.to_string()))?;

        if !status.is_success() {
            return Err(DeviceError::Remote(format!("TSS HTTP status {status}")));
        }

        parse_response(&text)
    }
}

/// Parse the TSS `key=value&...` response and decode `REQUEST_STRING` (recovered: `+` is escaped
/// to `%2B`, then it is parsed as a URL query, and its plist is decoded).
fn parse_response(text: &str) -> Result<Dictionary> {
    let mut message = None;
    let mut request_string = None;

    for pair in text.replace('+', "%2B").split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };

        let decoded = percent_decode(value);

        match key {
            "MESSAGE" => message = Some(decoded),
            "REQUEST_STRING" => request_string = Some(decoded),
            _ => {}
        }
    }

    if message.as_deref() != Some("SUCCESS") {
        let detail = message.unwrap_or_else(|| "no MESSAGE".into());

        return Err(DeviceError::Remote(format!("TSS responded: {detail}")));
    }

    let request_string =
        request_string.ok_or_else(|| DeviceError::Remote("TSS response has no REQUEST_STRING".into()))?;

    plist::from_bytes(request_string.as_bytes()).map_err(|error| DeviceError::Protocol(error.to_string()))
}

/// Minimal `application/x-www-form-urlencoded` decode for the TSS response fields.
fn percent_decode(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.bytes();

    while let Some(byte) = chars.next() {
        match byte {
            b'%' => {
                let high = chars.next();
                let low = chars.next();

                match (high, low) {
                    (Some(high), Some(low)) => match (hex(high), hex(low)) {
                        (Some(high), Some(low)) => bytes.push(high << 4 | low),
                        _ => {
                            bytes.push(b'%');
                            bytes.push(high);
                            bytes.push(low);
                        }
                    },
                    _ => bytes.push(b'%'),
                }
            }
            other => bytes.push(other),
        }
    }

    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Read an identifier that the personalization response reports as an integer.
fn identifier(identifiers: &Dictionary, key: &str) -> Result<u64> {
    identifiers
        .get(key)
        .and_then(Value::as_unsigned_integer)
        .ok_or_else(|| DeviceError::Protocol(format!("personalization identifiers missing {key}")))
}

/// Build the TSS request plist from the device identifiers, nonce and DDI build manifest.
///
/// This follows the recovered request: fixed `@` header tags, the device identity tags, the
/// production/security flags reported by the device, then the build-manifest components with
/// their `RestoreRequestRules` applied.
fn build_request(identifiers: &Dictionary, ap_nonce: Vec<u8>, build_manifest: &Dictionary) -> Result<Dictionary> {
    let board_id = identifier(identifiers, "BoardId")?;
    let chip_id = identifier(identifiers, "ChipID")?;
    let unique_chip_id = identifier(identifiers, "UniqueChipID")?;

    let production = identifiers.get("CertificateProductionStatus").and_then(Value::as_boolean).unwrap_or(true);
    let security_mode = identifiers.get("CertificateSecurityMode").and_then(Value::as_boolean).unwrap_or(true);
    let security_domain = identifiers.get("SecurityDomain").and_then(Value::as_unsigned_integer).unwrap_or(1);

    let mut request = Dictionary::new();

    request.insert("@HostPlatformInfo".into(), "mac".into());
    request.insert("@VersionInfo".into(), VERSION_INFO.into());
    request.insert("@UUID".into(), uuid::Uuid::new_v4().to_string().to_uppercase().into());
    request.insert("@ApImg4Ticket".into(), true.into());
    request.insert("@BBTicket".into(), true.into());

    request.insert("ApBoardID".into(), Value::from(board_id));
    request.insert("ApChipID".into(), Value::from(chip_id));
    request.insert("ApECID".into(), Value::from(unique_chip_id));
    request.insert("ApNonce".into(), Value::Data(ap_nonce));
    request.insert("SepNonce".into(), Value::Data(vec![0; 20]));
    request.insert("UID_MODE".into(), false.into());

    // Copy the `Ap,*` identity tags Apple wants echoed back (idevice does this; the recovered
    // client parses only the typed fields, see docs/DEVICE.md).
    for (key, value) in identifiers {
        if key.starts_with("Ap,") {
            request.insert(key.clone(), value.clone());
        }
    }

    let mut parameters = Dictionary::new();
    parameters.insert("ApProductionMode".into(), production.into());
    parameters.insert("ApSecurityMode".into(), security_mode.into());
    parameters.insert("ApSupportsImg4".into(), true.into());
    parameters.insert("ApSecurityDomain".into(), Value::from(security_domain));

    request.insert("ApProductionMode".into(), production.into());
    request.insert("ApSecurityMode".into(), security_mode.into());
    request.insert("ApSecurityDomain".into(), Value::from(security_domain));
    request.insert("ApSupportsImg4".into(), true.into());

    let build_identity = idevice::tss::select_build_identity(build_manifest, board_id, chip_id, None)
        .map_err(|error| DeviceError::Protocol(format!("no matching build identity: {error}")))?;

    let ddi_rules = build_identity
        .get("Manifest")
        .and_then(Value::as_dictionary)
        .and_then(|manifest| manifest.get("LoadableTrustCache"))
        .and_then(Value::as_dictionary)
        .and_then(|cache| cache.get("Info"))
        .and_then(Value::as_dictionary)
        .and_then(|info| info.get("RestoreRequestRules"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    populate_components(&mut request, build_identity, &parameters, &ddi_rules)?;

    Ok(request)
}

/// Append the trusted build-manifest components, each with the DDI `RestoreRequestRules` applied
/// (mirrors `idevice`'s `populate_from_manifest` with a rules override).
fn populate_components(
    request: &mut Dictionary,
    build_identity: &Dictionary,
    parameters: &Dictionary,
    rules: &[Value],
) -> Result<()> {
    let manifest = build_identity
        .get("Manifest")
        .and_then(Value::as_dictionary)
        .ok_or_else(|| DeviceError::Protocol("build identity has no Manifest".into()))?;

    for (key, item) in manifest {
        let Some(item) = item.as_dictionary() else {
            continue;
        };

        if !matches!(item.get("Trusted"), Some(Value::Boolean(true))) {
            continue;
        }

        let mut entry = item.clone();
        entry.remove("Info");

        idevice::tss::apply_restore_request_rules(&mut entry, parameters, rules);

        if !entry.contains_key("Digest") {
            entry.insert("Digest".into(), Value::Data(Vec::new()));
        }

        request.insert(key.clone(), Value::Dictionary(entry));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identifiers() -> Dictionary {
        let mut identifiers = Dictionary::new();
        identifiers.insert("BoardId".into(), Value::from(0x08u64));
        identifiers.insert("ChipID".into(), Value::from(0x8030u64));
        identifiers.insert("UniqueChipID".into(), Value::from(0x1122334455u64));
        identifiers.insert("CertificateProductionStatus".into(), true.into());
        identifiers.insert("CertificateSecurityMode".into(), true.into());
        identifiers.insert("SecurityDomain".into(), Value::from(1u64));
        identifiers.insert("Ap,OSLongVersion".into(), "23.1".into());
        identifiers
    }

    fn build_manifest() -> Vec<u8> {
        let rules = plist::Value::Array(vec![plist::Value::Dictionary({
            let mut rule = Dictionary::new();
            let mut conditions = Dictionary::new();
            conditions.insert("ApRawProductionMode".into(), true.into());
            let mut actions = Dictionary::new();
            actions.insert("EPRO".into(), true.into());
            rule.insert("Conditions".into(), Value::Dictionary(conditions));
            rule.insert("Actions".into(), Value::Dictionary(actions));
            rule
        })]);

        let mut info = Dictionary::new();
        info.insert("RestoreRequestRules".into(), rules);

        let mut trust_cache = Dictionary::new();
        trust_cache.insert("Trusted".into(), true.into());
        trust_cache.insert("Digest".into(), Value::Data(vec![1, 2, 3]));
        trust_cache.insert("Info".into(), Value::Dictionary(info));

        let mut manifest = Dictionary::new();
        manifest.insert("LoadableTrustCache".into(), Value::Dictionary(trust_cache));

        let mut identity = Dictionary::new();
        identity.insert("ApBoardID".into(), "0x08".into());
        identity.insert("ApChipID".into(), "0x8030".into());
        identity.insert("Manifest".into(), Value::Dictionary(manifest));

        let mut root = Dictionary::new();
        root.insert("BuildIdentities".into(), Value::Array(vec![Value::Dictionary(identity)]));

        let mut bytes = Vec::new();
        Value::Dictionary(root).to_writer_xml(&mut bytes).expect("xml");

        bytes
    }

    #[test]
    fn request_carries_recovered_identity_tags_and_applies_rules() {
        let manifest: Dictionary = plist::from_bytes(&build_manifest()).expect("manifest");
        let request = build_request(&identifiers(), vec![9, 9, 9, 9], &manifest).expect("request");

        assert_eq!(request.get("@HostPlatformInfo").and_then(Value::as_string), Some("mac"));
        assert_eq!(request.get("@VersionInfo").and_then(Value::as_string), Some(VERSION_INFO));
        assert_eq!(request.get("ApBoardID").and_then(Value::as_unsigned_integer), Some(0x08));
        assert_eq!(request.get("ApChipID").and_then(Value::as_unsigned_integer), Some(0x8030));
        assert_eq!(request.get("ApECID").and_then(Value::as_unsigned_integer), Some(0x1122334455));
        assert_eq!(request.get("SepNonce").and_then(Value::as_data).map(<[u8]>::len), Some(20));
        assert_eq!(request.get("ApNonce").and_then(Value::as_data), Some([9, 9, 9, 9].as_slice()));
        assert_eq!(request.get("Ap,OSLongVersion").and_then(Value::as_string), Some("23.1"));

        let trust_cache = request.get("LoadableTrustCache").and_then(Value::as_dictionary).expect("component");
        assert!(trust_cache.get("Info").is_none(), "Info is stripped from TSS components");
        assert_eq!(trust_cache.get("EPRO").and_then(Value::as_boolean), Some(true), "the rule action applied");
    }

    #[test]
    fn parse_response_decodes_the_request_string_plist() {
        let mut ticket = Dictionary::new();
        ticket.insert("ApImg4Ticket".into(), Value::Data(vec![0xAB, 0xCD]));

        let mut xml = Vec::new();
        Value::Dictionary(ticket).to_writer_xml(&mut xml).expect("xml");

        let encoded = xml.iter().map(|byte| format!("%{byte:02X}")).collect::<String>();
        let body = format!("STATUS=0&MESSAGE=SUCCESS&REQUEST_STRING={encoded}");

        let parsed = parse_response(&body).expect("parsed");
        assert_eq!(parsed.get("ApImg4Ticket").and_then(Value::as_data), Some([0xAB, 0xCD].as_slice()));
    }

    #[test]
    fn parse_response_surfaces_a_non_success_message() {
        let error = parse_response("STATUS=94&MESSAGE=This%20device%20isn%27t%20eligible");

        assert!(matches!(error, Err(DeviceError::Remote(_))), "{error:?}");
    }
}
