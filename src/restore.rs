//! The `restore_data` the frontend keeps in its permission store for
//! sessions started with a persist_mode: `(suv)`, here
//! `("omarchy", 1, {"app_id": <s>, "clipboard": <b>})`. The frontend hands
//! the app a restore_token for it and gives the data back to SelectDevices
//! when the app presents that token, which is how "Always allow" skips the
//! dialog.

use std::collections::HashMap;

use zbus::zvariant::{OwnedValue, Value};

const VENDOR: &str = "omarchy";
const VERSION: u32 = 1;

/// The restore_data recording that `app_id` may use the clipboard.
pub fn encode(app_id: &str) -> OwnedValue {
    let mut grant: HashMap<&str, Value<'_>> = HashMap::new();
    grant.insert("app_id", Value::from(app_id));
    grant.insert("clipboard", Value::from(true));
    let data = Value::from((VENDOR, VERSION, Value::from(grant)));
    OwnedValue::try_from(data).expect("restore data holds no file descriptors")
}

/// Whether `value` (a restore_data from the frontend) grants `app_id` the
/// clipboard. Anything else — another vendor or version, another app, a
/// malformed value — doesn't.
pub fn grants_clipboard(value: &Value<'_>, app_id: &str) -> bool {
    let Value::Structure(structure) = value else {
        return false;
    };
    let [Value::Str(vendor), Value::U32(version), Value::Value(inner)] = structure.fields() else {
        return false;
    };
    if vendor.as_str() != VENDOR || *version != VERSION {
        return false;
    }
    let Value::Dict(dict) = inner.as_ref() else {
        return false;
    };
    let granted_app = dict.get::<&str, &str>(&"app_id").ok().flatten();
    let clipboard = dict.get::<&str, bool>(&"clipboard").ok().flatten();
    granted_app == Some(app_id) && clipboard == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let data = encode("com.bitwarden.desktop");
        assert_eq!(data.value_signature(), "(suv)");
        assert!(grants_clipboard(&data, "com.bitwarden.desktop"));
    }

    #[test]
    fn is_per_app() {
        assert!(!grants_clipboard(
            &encode("com.bitwarden.desktop"),
            "org.example.Other"
        ));
    }

    #[test]
    fn rejects_other_vendors_and_shapes() {
        let mut grant: HashMap<&str, Value<'_>> = HashMap::new();
        grant.insert("app_id", Value::from("a"));
        grant.insert("clipboard", Value::from(true));
        let other_vendor = Value::from(("hyprland", 1u32, Value::from(grant)));
        assert!(!grants_clipboard(&other_vendor, "a"));
        assert!(!grants_clipboard(&Value::from("omarchy"), "a"));
        assert!(!grants_clipboard(
            &Value::from(("omarchy", 1u32, Value::from(1u32))),
            "a"
        ));
    }
}
