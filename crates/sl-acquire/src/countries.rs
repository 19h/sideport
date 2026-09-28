//! App Store countries accepted by `sideloadly://` deeplinks (`c=`), in the recovered order.
//!
//! The 155 ISO 3166-1 alpha-2 codes and display names were read from the
//! `sideloadly/appstore/countries` table of Sideloadly 0.60's Go binary. Storefront identifiers
//! are not part of that table; the Store client learns them from server responses.

/// `(code, name)` pairs.
pub const COUNTRIES: &[(&str, &str)] = &[
    ("AL", "Albania"),
    ("DZ", "Algeria"),
    ("AO", "Angola"),
    ("AI", "Anguilla"),
    ("AG", "Antigua and Barbuda"),
    ("AR", "Argentina"),
    ("AM", "Armenia"),
    ("AU", "Australia"),
    ("AT", "Austria"),
    ("AZ", "Azerbaijan"),
    ("BS", "Bahamas"),
    ("BH", "Bahrain"),
    ("BB", "Barbados"),
    ("BY", "Belarus"),
    ("BE", "Belgium"),
    ("BZ", "Belize"),
    ("BJ", "Benin"),
    ("BM", "Bermuda"),
    ("BT", "Bhutan"),
    ("BO", "Bolivia"),
    ("BW", "Botswana"),
    ("BR", "Brazil"),
    ("VG", "British Virgin Islands"),
    ("BN", "Brunei Darussalam"),
    ("BG", "Bulgaria"),
    ("BF", "Burkina-Faso"),
    ("KH", "Cambodia"),
    ("CA", "Canada"),
    ("CV", "Cape Verde"),
    ("KY", "Cayman Islands"),
    ("TD", "Chad"),
    ("CL", "Chile"),
    ("CN", "China"),
    ("CO", "Colombia"),
    ("CR", "Costa Rica"),
    ("HR", "Croatia"),
    ("CY", "Cyprus"),
    ("CZ", "Czech Republic"),
    ("CG", "Democratic Republic of the Congo"),
    ("DK", "Denmark"),
    ("DM", "Dominica"),
    ("DO", "Dominican Republic"),
    ("EC", "Ecuador"),
    ("EG", "Egypt"),
    ("SV", "El Salvador"),
    ("EE", "Estonia"),
    ("FM", "Federated States of Micronesia"),
    ("FJ", "Fiji"),
    ("FI", "Finland"),
    ("FR", "France"),
    ("GM", "Gambia"),
    ("DE", "Germany"),
    ("GH", "Ghana"),
    ("GB", "Great Britain"),
    ("GR", "Greece"),
    ("GD", "Grenada"),
    ("GT", "Guatemala"),
    ("GW", "Guinea Bissau"),
    ("GY", "Guyana"),
    ("HN", "Honduras"),
    ("HK", "Hong Kong"),
    ("HU", "Hungaria"),
    ("IS", "Iceland"),
    ("IN", "India"),
    ("ID", "Indonesia"),
    ("IE", "Ireland"),
    ("IL", "Israel"),
    ("IT", "Italy"),
    ("JM", "Jamaica"),
    ("JP", "Japan"),
    ("JO", "Jordan"),
    ("KZ", "Kazakhstan"),
    ("KE", "Kenya"),
    ("KG", "Krygyzstan"),
    ("KW", "Kuwait"),
    ("LA", "Laos"),
    ("LV", "Latvia"),
    ("LB", "Lebanon"),
    ("LR", "Liberia"),
    ("LT", "Lithuania"),
    ("LU", "Luxembourg"),
    ("MO", "Macau"),
    ("MK", "Macedonia"),
    ("MG", "Madagascar"),
    ("MW", "Malawi"),
    ("MY", "Malaysia"),
    ("ML", "Mali"),
    ("MT", "Malta"),
    ("MR", "Mauritania"),
    ("MU", "Mauritius"),
    ("MX", "Mexico"),
    ("MD", "Moldova"),
    ("MN", "Mongolia"),
    ("MS", "Montserrat"),
    ("MZ", "Mozambique"),
    ("NA", "Namibia"),
    ("NP", "Nepal"),
    ("NL", "Netherlands"),
    ("NZ", "New Zealand"),
    ("NI", "Nicaragua"),
    ("NE", "Niger"),
    ("NG", "Nigeria"),
    ("NO", "Norway"),
    ("OM", "Oman"),
    ("PK", "Pakistan"),
    ("PW", "Palau"),
    ("PA", "Panama"),
    ("PG", "Papua New Guinea"),
    ("PY", "Paraguay"),
    ("PE", "Peru"),
    ("PH", "Philippines"),
    ("PL", "Poland"),
    ("PT", "Portugal"),
    ("QA", "Qatar"),
    ("TT", "Republic of Trinidad and Tobago"),
    ("RO", "Romania"),
    ("RU", "Russia"),
    ("KN", "Saint Kitts and Nevis"),
    ("LC", "Saint Lucia"),
    ("VC", "Saint Vincent and the Grenadines"),
    ("ST", "Sao Tome e Principe"),
    ("SA", "Saudi Arabia"),
    ("SN", "Senegal"),
    ("SC", "Seychelles"),
    ("SL", "Sierra Leone"),
    ("SG", "Singapore"),
    ("SK", "Slovakia"),
    ("SI", "Slovenia"),
    ("SB", "Soloman Islands"),
    ("ZA", "South Africa"),
    ("KR", "South Korea"),
    ("ES", "Spain"),
    ("LK", "Sri Lanka"),
    ("SR", "Suriname"),
    ("SZ", "Swaziland"),
    ("SE", "Sweden"),
    ("CH", "Switzerland"),
    ("TW", "Taiwan"),
    ("TJ", "Tajikistan"),
    ("TZ", "Tanzania"),
    ("TH", "Thailand"),
    ("TN", "Tunisia"),
    ("TR", "Turkey"),
    ("TM", "Turkmenistan"),
    ("TC", "Turks and Caicos Islands"),
    ("UG", "Uganda"),
    ("UA", "Ukraine"),
    ("AE", "United Arab Emirates"),
    ("US", "United States of America"),
    ("UY", "Uruguay"),
    ("UZ", "Uzbekistan"),
    ("VE", "Venezuela"),
    ("VN", "Vietnam"),
    ("YE", "Yemen"),
    ("ZW", "Zimbabwe"),
];

/// The country for a case-insensitive code (recovered `CountryFromCode(strings.ToUpper(c))`).
pub fn country(code: &str) -> Option<Country> {
    let code = code.to_ascii_uppercase();

    COUNTRIES.iter().find(|(candidate, _)| *candidate == code).map(|(code, name)| Country { code, name })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Country {
    pub code: &'static str,
    pub name: &'static str,
}

impl std::fmt::Display for Country {
    /// Recovered `Country.String`: `CODE - Name`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} - {}", self.code, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recovered_table_has_unique_codes_and_case_insensitive_lookup() {
        let mut codes: Vec<_> = COUNTRIES.iter().map(|(code, _)| *code).collect();
        codes.sort_unstable();
        codes.dedup();

        assert_eq!(COUNTRIES.len(), 155);
        assert_eq!(codes.len(), 155);
        assert_eq!(country("us").map(|country| country.to_string()).as_deref(), Some("US - United States of America"));
        assert!(country("XX").is_none());
    }
}
