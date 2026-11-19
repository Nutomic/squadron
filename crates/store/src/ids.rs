use base64::Engine;
use uuid::Uuid;

pub fn new_run_id() -> String {
    let id = Uuid::new_v4();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id.as_bytes())
}

pub fn is_valid_run_id(id: &str) -> bool {
    id.len() == 22
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_url_safe_unguessable_and_unique() {
        let ids: HashSet<String> = (0..1000).map(|_| new_run_id()).collect();
        assert_eq!(ids.len(), 1000);
        for id in &ids {
            assert_eq!(id.len(), 22);
            assert!(is_valid_run_id(id));
            assert!(!id.contains('/'));
            assert!(!id.contains('+'));
            assert!(!id.contains('='));
        }
        assert!(!is_valid_run_id("short"));
    }
}
