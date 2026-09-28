//! Marketing names for `ProductType` identifiers, for display only.
//!
//! Unknown identifiers return `None`; callers show the identifier itself.

const MODELS: &[(&str, &str)] = &[
    ("iPhone8,1", "iPhone 6s"),
    ("iPhone8,2", "iPhone 6s Plus"),
    ("iPhone8,4", "iPhone SE"),
    ("iPhone9,1", "iPhone 7"),
    ("iPhone9,3", "iPhone 7"),
    ("iPhone9,2", "iPhone 7 Plus"),
    ("iPhone9,4", "iPhone 7 Plus"),
    ("iPhone10,1", "iPhone 8"),
    ("iPhone10,4", "iPhone 8"),
    ("iPhone10,2", "iPhone 8 Plus"),
    ("iPhone10,5", "iPhone 8 Plus"),
    ("iPhone10,3", "iPhone X"),
    ("iPhone10,6", "iPhone X"),
    ("iPhone11,2", "iPhone XS"),
    ("iPhone11,4", "iPhone XS Max"),
    ("iPhone11,6", "iPhone XS Max"),
    ("iPhone11,8", "iPhone XR"),
    ("iPhone12,1", "iPhone 11"),
    ("iPhone12,3", "iPhone 11 Pro"),
    ("iPhone12,5", "iPhone 11 Pro Max"),
    ("iPhone12,8", "iPhone SE (2nd generation)"),
    ("iPhone13,1", "iPhone 12 mini"),
    ("iPhone13,2", "iPhone 12"),
    ("iPhone13,3", "iPhone 12 Pro"),
    ("iPhone13,4", "iPhone 12 Pro Max"),
    ("iPhone14,2", "iPhone 13 Pro"),
    ("iPhone14,3", "iPhone 13 Pro Max"),
    ("iPhone14,4", "iPhone 13 mini"),
    ("iPhone14,5", "iPhone 13"),
    ("iPhone14,6", "iPhone SE (3rd generation)"),
    ("iPhone14,7", "iPhone 14"),
    ("iPhone14,8", "iPhone 14 Plus"),
    ("iPhone15,2", "iPhone 14 Pro"),
    ("iPhone15,3", "iPhone 14 Pro Max"),
    ("iPhone15,4", "iPhone 15"),
    ("iPhone15,5", "iPhone 15 Plus"),
    ("iPhone16,1", "iPhone 15 Pro"),
    ("iPhone16,2", "iPhone 15 Pro Max"),
    ("iPhone17,1", "iPhone 16 Pro"),
    ("iPhone17,2", "iPhone 16 Pro Max"),
    ("iPhone17,3", "iPhone 16"),
    ("iPhone17,4", "iPhone 16 Plus"),
    ("iPhone17,5", "iPhone 16e"),
    ("iPod9,1", "iPod touch (7th generation)"),
    ("iPad7,5", "iPad (6th generation)"),
    ("iPad7,6", "iPad (6th generation)"),
    ("iPad7,11", "iPad (7th generation)"),
    ("iPad7,12", "iPad (7th generation)"),
    ("iPad11,6", "iPad (8th generation)"),
    ("iPad11,7", "iPad (8th generation)"),
    ("iPad12,1", "iPad (9th generation)"),
    ("iPad12,2", "iPad (9th generation)"),
    ("iPad13,18", "iPad (10th generation)"),
    ("iPad13,19", "iPad (10th generation)"),
    ("iPad11,3", "iPad Air (3rd generation)"),
    ("iPad11,4", "iPad Air (3rd generation)"),
    ("iPad13,1", "iPad Air (4th generation)"),
    ("iPad13,2", "iPad Air (4th generation)"),
    ("iPad13,16", "iPad Air (5th generation)"),
    ("iPad13,17", "iPad Air (5th generation)"),
    ("iPad11,1", "iPad mini (5th generation)"),
    ("iPad11,2", "iPad mini (5th generation)"),
    ("iPad14,1", "iPad mini (6th generation)"),
    ("iPad14,2", "iPad mini (6th generation)"),
    ("iPad8,1", "iPad Pro 11-inch"),
    ("iPad8,9", "iPad Pro 11-inch (2nd generation)"),
    ("iPad13,4", "iPad Pro 11-inch (3rd generation)"),
    ("iPad13,8", "iPad Pro 12.9-inch (5th generation)"),
    ("iPad14,3", "iPad Pro 11-inch (4th generation)"),
    ("iPad14,5", "iPad Pro 12.9-inch (6th generation)"),
    ("AppleTV5,3", "Apple TV HD"),
    ("AppleTV6,2", "Apple TV 4K"),
    ("AppleTV11,1", "Apple TV 4K (2nd generation)"),
    ("AppleTV14,1", "Apple TV 4K (3rd generation)"),
];

pub fn marketing_name(product_type: &str) -> Option<&'static str> {
    MODELS.iter().find(|(identifier, _)| *identifier == product_type).map(|(_, name)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_unique_and_unknown_ones_are_not_named() {
        let mut identifiers: Vec<_> = MODELS.iter().map(|(identifier, _)| *identifier).collect();
        identifiers.sort_unstable();
        identifiers.dedup();

        assert_eq!(identifiers.len(), MODELS.len());
        assert_eq!(marketing_name("iPhone15,2"), Some("iPhone 14 Pro"));
        assert_eq!(marketing_name("iPhone99,9"), None);
    }
}
