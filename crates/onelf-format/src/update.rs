//! Update-URL conventions shared by the runtime and the packer.
//!
//! The runtime derives the detached-signature URL from the update URL,
//! and the packer has to predict that name so a publisher uploads the
//! signature where it will actually be requested. Both sides deriving it
//! from one function is the point: a signature published under a name the
//! runtime never asks for fails silently, and only for users who try to
//! update.

/// Build the detached-signature URL by appending `.sig` to the path
/// component, before any query string or fragment. `https://h/a?t=1`
/// becomes `https://h/a.sig?t=1`, preserving query-bearing update URLs.
pub fn detached_sig_url(url: &str) -> String {
    let split = url.find(['?', '#']).unwrap_or(url.len());
    let (path, rest) = url.split_at(split);
    format!("{path}.sig{rest}")
}

/// The bare filename a publisher must upload the signature as, derived
/// from the update URL the package carries.
pub fn detached_sig_filename(url: &str) -> Option<String> {
    let sig_url = detached_sig_url(url);
    let path = sig_url.split(['?', '#']).next()?;
    let name = path.rsplit('/').next()?;
    if name.is_empty() || name == ".sig" {
        // A URL whose path ends in `/` derives the degenerate hidden name
        // `.sig`; that names nothing a publisher can upload, so fall back
        // to `<file>.sig` instead.
        return None;
    }
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sig_url_appends_to_the_path_component() {
        assert_eq!(
            detached_sig_url("https://h/app.onelf"),
            "https://h/app.onelf.sig"
        );
    }

    #[test]
    fn sig_url_preserves_a_query_string() {
        assert_eq!(
            detached_sig_url("https://h/app.onelf?t=1"),
            "https://h/app.onelf.sig?t=1"
        );
        assert_eq!(
            detached_sig_url("https://h/app.onelf#frag"),
            "https://h/app.onelf.sig#frag"
        );
    }

    #[test]
    fn filename_is_what_the_publisher_uploads() {
        assert_eq!(
            detached_sig_filename("https://h/d/app.onelf").as_deref(),
            Some("app.onelf.sig")
        );
        assert_eq!(
            detached_sig_filename("https://h/d/app.onelf?v=2").as_deref(),
            Some("app.onelf.sig")
        );
    }

    #[test]
    fn a_trailing_slash_url_falls_back_instead_of_a_hidden_dotfile() {
        assert_eq!(detached_sig_filename("https://h/app.onelf/"), None);
        assert_eq!(detached_sig_filename("https://h/"), None);
        // The URL itself is still formed the same way; only the derived
        // file name falls back.
        assert_eq!(
            detached_sig_url("https://h/app.onelf/"),
            "https://h/app.onelf/.sig"
        );
    }
}
