//! `sideloadly:` links (recovered `urischeme.NewRemoteFile`).
//!
//! - `sideloadly:<url>`: the opaque part (with its query) is the download URL.
//! - `sideloadly:?dn=<name>&xs=<ipa url>&h=<md5|sha1 hex>&metadata=…&sinfs=…&artwork=<url>`.
//! - Without `xs`, an App Store deeplink: `c=<country>&bi=<bundle id>[&v=<version>]`.
//!
//! Any other path is rejected, as the recovered parser does.

use crate::countries::{self, Country};
use crate::{Error, Result};
use url::Url;

/// Expected digest of a download (recovered `h`: 16 bytes MD5 or 20 bytes SHA-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Digest {
    Md5([u8; 16]),
    Sha1([u8; 20]),
}

/// App Store enrichment carried by a link (recovered `ipa.EnrichIpa` inputs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Enrichment {
    /// Written verbatim as `iTunesMetadata.plist`.
    pub metadata: Option<String>,
    /// Base64 SINF written to `Payload/<app>.app/SC_Info/<app>.sinf`.
    pub sinfs: Option<String>,
    /// Artwork URL fetched into `iTunesArtwork`.
    pub artwork: Option<String>,
}

impl Enrichment {
    pub fn is_empty(&self) -> bool {
        self.metadata.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Download { url: String, name: Option<String>, digest: Option<Digest>, enrichment: Enrichment },
    AppStore { country: Country, bundle_id: String, version: Option<String> },
}

impl Link {
    pub fn parse(text: &str) -> Result<Self> {
        let parsed = Url::parse(text).map_err(|error| Error::Link(format!("Bad URL: {text} - {error}")))?;

        if parsed.scheme() != "sideloadly" {
            return Err(Error::Link(format!("Not a valid Sideloadly URL: {text}")));
        }

        if parsed.cannot_be_a_base() && !parsed.path().is_empty() {
            let url = match parsed.query() {
                Some(query) => format!("{}?{query}", parsed.path()),
                None => parsed.path().to_owned(),
            };

            return Ok(Self::Download { url, name: None, digest: None, enrichment: Enrichment::default() });
        }

        // Go's parser accepts only an empty path here (`sideloadly:?…` or `sideloadly://?…`).
        if !parsed.path().is_empty() {
            return Err(Error::Link(format!("Invalid Sideloadly URL: {text}")));
        }

        let query = |key: &str| {
            parsed
                .query_pairs()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.into_owned())
                .filter(|value| !value.is_empty())
        };

        let digest = query("h")
            .map(|hex| decode_digest(&hex).ok_or_else(|| Error::Link(format!("Improper Sideloadly URL: {text}"))))
            .transpose()?;
        let enrichment = Enrichment { metadata: query("metadata"), sinfs: query("sinfs"), artwork: query("artwork") };

        if let Some(url) = query("xs") {
            return Ok(Self::Download { url, name: query("dn"), digest, enrichment });
        }

        let country = query("c").and_then(|code| countries::country(&code));
        let bundle_id = query("bi");

        match (country, bundle_id) {
            (Some(country), Some(bundle_id)) => Ok(Self::AppStore { country, bundle_id, version: query("v") }),
            _ => Err(Error::Link(format!("Incorrect Sideloadly URL: {text}"))),
        }
    }

    /// Recovered `BeautifulName`: the last non-empty URL path component (or the host when the
    /// path is `/` or `.`), else `<bundle id><version>.ipa` for App Store links.
    pub fn file_name(&self) -> String {
        match self {
            Self::Download { name: Some(name), .. } => name.clone(),

            Self::Download { url, .. } => {
                let Ok(parsed) = Url::parse(url) else {
                    return last_component(url);
                };

                let component = last_component(parsed.path());

                if component == "/" || component == "." {
                    parsed.host_str().map(str::to_owned).unwrap_or_else(|| last_component(url))
                } else {
                    component
                }
            }

            Self::AppStore { bundle_id, version, .. } => {
                format!("{bundle_id}{}.ipa", version.as_deref().unwrap_or_default())
            }
        }
    }
}

/// Go `path.Base` semantics without the leading-path cleanup: trailing slashes are removed, an
/// empty input is `.`, and an all-slash input is `/`.
fn last_component(text: &str) -> String {
    if text.is_empty() {
        return ".".into();
    }

    let trimmed = text.trim_end_matches('/');

    if trimmed.is_empty() {
        return "/".into();
    }

    trimmed.rsplit('/').next().filter(|component| !component.is_empty()).unwrap_or("/").to_owned()
}

fn decode_digest(text: &str) -> Option<Digest> {
    let bytes = hex::decode(text).ok()?;

    match bytes.len() {
        16 => Some(Digest::Md5(bytes.try_into().ok()?)),
        20 => Some(Digest::Sha1(bytes.try_into().ok()?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_links_carry_the_url_and_its_query() {
        let link = Link::parse("sideloadly:https://example.com/apps/Game.ipa?token=1").expect("opaque link");

        assert_eq!(
            link,
            Link::Download {
                url: "https://example.com/apps/Game.ipa?token=1".into(),
                name: None,
                digest: None,
                enrichment: Enrichment::default()
            }
        );
        assert_eq!(link.file_name(), "Game.ipa");
    }

    #[test]
    fn query_links_decode_name_digest_and_enrichment() {
        let sha1 = "a9993e364706816aba3e25717850c26c9cd0d89d";
        let text = format!(
            "sideloadly:?dn=My%20App.ipa&xs=https%3A%2F%2Fcdn.example%2Fa.ipa&h={sha1}&metadata=%3Cplist%2F%3E&sinfs=AAAA&artwork=https%3A%2F%2Fcdn.example%2Fart.png"
        );
        let link = Link::parse(&text).expect("query link");

        let Link::Download { url, name, digest, enrichment } = &link else { panic!("download link") };
        assert_eq!(url, "https://cdn.example/a.ipa");
        assert_eq!(name.as_deref(), Some("My App.ipa"));
        assert!(matches!(digest, Some(Digest::Sha1(bytes)) if hex::encode(bytes) == sha1));
        assert_eq!(enrichment.metadata.as_deref(), Some("<plist/>"));
        assert_eq!(enrichment.artwork.as_deref(), Some("https://cdn.example/art.png"));
        assert_eq!(link.file_name(), "My App.ipa");

        let md5 = Link::parse("sideloadly:?xs=https://h/x.ipa&h=900150983cd24fb0d6963f7d28e17f72").expect("md5");
        assert!(matches!(md5, Link::Download { digest: Some(Digest::Md5(_)), .. }));
    }

    #[test]
    fn app_store_deeplinks_require_a_known_country_and_bundle_id() {
        let link = Link::parse("sideloadly:?c=de&bi=com.example.app&v=1.2").expect("deeplink");

        assert!(matches!(&link, Link::AppStore { country, bundle_id, version: Some(version) }
            if country.code == "DE" && bundle_id == "com.example.app" && version == "1.2"));
        assert_eq!(link.file_name(), "com.example.app1.2.ipa");

        for text in ["sideloadly:?c=XX&bi=com.example.app", "sideloadly:?c=US", "sideloadly:?bi=com.example.app"] {
            assert!(
                matches!(Link::parse(text), Err(Error::Link(message)) if message.starts_with("Incorrect")),
                "{text}"
            );
        }
    }

    #[test]
    fn malformed_links_use_the_recovered_messages() {
        let cases = [
            ("https://example.com/a.ipa", "Not a valid Sideloadly URL"),
            ("sideloadly:?xs=https://h/a.ipa&h=abcd", "Improper Sideloadly URL"),
            ("sideloadly:?xs=https://h/a.ipa&h=zz", "Improper Sideloadly URL"),
            ("sideloadly://host/path", "Invalid Sideloadly URL"),
            ("sideloadly:///?xs=https://h/a.ipa", "Invalid Sideloadly URL"),
            ("not a url", "Bad URL"),
        ];

        for (text, prefix) in cases {
            assert!(matches!(Link::parse(text), Err(Error::Link(message)) if message.starts_with(prefix)), "{text}");
        }
    }

    #[test]
    fn file_names_follow_go_path_base() {
        let name = |url: &str| {
            Link::Download { url: url.into(), name: None, digest: None, enrichment: Enrichment::default() }.file_name()
        };

        assert_eq!(name("https://example.com/dir/App.ipa/"), "App.ipa");
        assert_eq!(name("https://example.com/"), "example.com");
        assert_eq!(name("https://example.com"), "example.com");
    }
}
