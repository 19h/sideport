use plist::Value;
use proptest::prelude::*;
use sl_bundle::read_dictionary;
use std::fs;

#[test]
fn openstep_comments_nested_values_escapes_and_utf16_are_decoded() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let path = temporary.path().join("InfoPlist.strings");
    let text = r#"/* comment */ {
        Name = "Line\n\uD83D\uDE80";
        Nested = { Data = <00 ff 12>; List = (one, "two",); };
        // another comment
        Quote = "a\"b";
    }"#;

    fs::write(&path, text).expect("UTF8");
    let utf8 = read_dictionary(&path).expect("OpenStep");

    assert_eq!(utf8["Name"].as_string(), Some("Line\n🚀"));
    assert_eq!(utf8["Quote"].as_string(), Some("a\"b"));
    assert_eq!(utf8["Nested"].as_dictionary().expect("nested")["Data"].as_data(), Some(&[0, 255, 18][..]));

    for little in [false, true] {
        let mut bytes = if little { vec![0xff, 0xfe] } else { vec![0xfe, 0xff] };

        for word in text.encode_utf16() {
            bytes.extend(if little { word.to_le_bytes() } else { word.to_be_bytes() });
        }

        fs::write(&path, bytes).expect("UTF16");

        assert_eq!(read_dictionary(&path).expect("UTF16 OpenStep"), utf8);
    }

    for little in [false, true] {
        let mut bytes = if little { vec![0xff, 0xfe, 0, 0] } else { vec![0, 0, 0xfe, 0xff] };

        for character in text.chars() {
            bytes.extend(if little { (character as u32).to_le_bytes() } else { (character as u32).to_be_bytes() });
        }

        fs::write(&path, bytes).expect("UTF32");

        assert_eq!(read_dictionary(&path).expect("UTF32 OpenStep"), utf8);
    }

    Value::Dictionary(utf8.clone()).to_file_binary(&path).expect("binary plist");

    assert_eq!(read_dictionary(&path).expect("binary"), utf8);

    let mut xml = Vec::new();
    Value::Dictionary(utf8.clone()).to_writer_xml(&mut xml).expect("XML");
    let xml = String::from_utf8(xml).expect("XML text").replacen("UTF-8", "UTF-16", 1);
    let mut encoded = vec![0xff, 0xfe];

    for word in xml.encode_utf16() {
        encoded.extend(word.to_le_bytes());
    }

    fs::write(&path, encoded).expect("UTF16 XML");

    assert_eq!(read_dictionary(&path).expect("UTF16 XML"), utf8);
}

#[test]
fn incomplete_duplicate_and_excessively_nested_input_is_rejected() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let path = temporary.path().join("InfoPlist.strings");

    for text in
        [r#""Key" = "unterminated"#, "/* missing end", "Key = x; Key = y;", "Key = <abc>;", r#"Key = "\uD800";"#]
    {
        fs::write(&path, text).expect("text");

        assert!(read_dictionary(&path).is_err(), "{text}");
    }

    let deep = format!("Key = {}x{};", "(".repeat(70), ")".repeat(70));
    fs::write(&path, deep).expect("deep");

    assert!(read_dictionary(&path).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn arbitrary_property_lists_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let temporary = tempfile::tempdir().expect("tempdir");
        let path = temporary.path().join("Info.plist");
        fs::write(&path, bytes).expect("input");

        let _ = read_dictionary(&path);
    }
}
