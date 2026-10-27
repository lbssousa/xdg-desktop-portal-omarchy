//! The dialog's catalogs (plugin/i18n/) against the English one: the same
//! keys, no empty strings and the same %N placeholders, so that a missing
//! or mangled translation fails here instead of showing a raw key or
//! dropping the app's name.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn catalog(path: &Path) -> BTreeMap<String, String> {
    let raw = fs::read_to_string(path).unwrap();
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn placeholders(text: &str) -> Vec<&str> {
    let mut found: Vec<&str> = text
        .match_indices('%')
        .filter_map(|(i, _)| {
            let digits = text[i + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .count();
            (digits > 0).then(|| &text[i..i + 1 + digits])
        })
        .collect();
    found.sort_unstable();
    found
}

#[test]
fn catalogs_match_english() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin/i18n");
    let english = catalog(&dir.join("en.json"));
    assert!(!english.is_empty());
    let mut count = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with(".json"), "{name}: not a catalog");
        let strings = catalog(&path);
        assert_eq!(
            strings.keys().collect::<Vec<_>>(),
            english.keys().collect::<Vec<_>>(),
            "{name}: keys"
        );
        for (key, text) in &strings {
            assert!(!text.trim().is_empty(), "{name}: {key} is empty");
            assert_eq!(
                placeholders(text),
                placeholders(&english[key]),
                "{name}: {key} placeholders"
            );
        }
        count += 1;
    }
    assert!(count > 1);
}
