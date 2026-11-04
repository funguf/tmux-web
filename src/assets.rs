//! Embedded static web assets (owned by the server unit).
//!
//! `web/` is embedded into release builds (debug builds read it from disk). The `rust-embed`
//! `exclude` attribute needs a cargo feature the manifest does not enable, so declaration files
//! (`*.d.ts`) are filtered here instead: they are never served.

use crate::security::percent_decode;
use rust_embed::RustEmbed;
use std::borrow::Cow;

#[derive(RustEmbed)]
#[folder = "web/"]
struct WebAssets;

/// The entry page, served for `/`.
pub const INDEX: &str = "index.html";

/// One servable file.
pub struct Asset {
    pub data: Cow<'static, [u8]>,
    pub content_type: String,
    /// Strong validator derived from the content hash, quoted as a header value.
    pub etag: String,
}

/// Looks up the asset for a request path such as `/`, `/app.js` or `/vendor/xterm/xterm.mjs`.
/// `None` for anything that is missing, hidden (`*.d.ts`) or not a plain relative path.
pub fn lookup(request_path: &str) -> Option<Asset> {
    let path = embedded_path(request_path)?;
    if is_hidden(&path) {
        return None;
    }
    let file = WebAssets::get(&path)?;
    let hash = file.metadata.sha256_hash();
    Some(Asset {
        content_type: content_type(&path),
        etag: format!("\"{}\"", hex::encode(&hash[..8])),
        data: file.data,
    })
}

/// Maps a request path to a path inside `web/`: `/` becomes the index page; anything that could
/// leave the folder (`..`, empty or backslash segments, NUL or control characters) is refused.
fn embedded_path(request_path: &str) -> Option<String> {
    let decoded = percent_decode(request_path);
    let relative = decoded.strip_prefix('/')?;
    if relative.is_empty() {
        return Some(INDEX.to_string());
    }
    let clean = relative.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && !segment.contains('\\')
            && !segment.chars().any(char::is_control)
    });
    clean.then(|| relative.to_string())
}

/// TypeScript declaration files ship next to the vendored libraries but are not part of the app.
fn is_hidden(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".d.ts")
}

/// `Content-Type` for a file name. JavaScript is always `text/javascript; charset=utf-8`
/// (`.mjs` included, so browsers accept it as a module).
pub fn content_type(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    match extension.as_deref() {
        Some("js" | "mjs") => "text/javascript; charset=utf-8".to_string(),
        Some("css") => "text/css; charset=utf-8".to_string(),
        Some("html" | "htm") => "text/html; charset=utf-8".to_string(),
        Some("json" | "map") => "application/json".to_string(),
        Some("svg") => "image/svg+xml".to_string(),
        Some("wasm") => "application/wasm".to_string(),
        Some("woff2") => "font/woff2".to_string(),
        Some("woff") => "font/woff".to_string(),
        Some("txt") => "text/plain; charset=utf-8".to_string(),
        // Extensionless files in web/ are licence texts.
        None => "text/plain; charset=utf-8".to_string(),
        Some(_) => match mime_guess::from_path(name).first() {
            Some(mime) if mime.type_() == mime_guess::mime::TEXT => {
                format!("{}; charset=utf-8", mime.essence_str())
            }
            Some(mime) => mime.essence_str().to_string(),
            None => "application/octet-stream".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn javascript_and_modules_are_text_javascript() {
        for name in ["app.js", "vendor/xterm/xterm.mjs", "A.JS", "x/y/z.MJS"] {
            assert_eq!(
                content_type(name),
                "text/javascript; charset=utf-8",
                "{name}"
            );
        }
    }

    #[test]
    fn common_types_have_the_expected_content_types() {
        for (name, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("style.css", "text/css; charset=utf-8"),
            ("data.json", "application/json"),
            ("app.js.map", "application/json"),
            ("icon.svg", "image/svg+xml"),
            ("font.woff2", "font/woff2"),
            ("font.woff", "font/woff"),
            ("logo.png", "image/png"),
            ("photo.jpg", "image/jpeg"),
            ("favicon.ico", "image/x-icon"),
            ("a.wasm", "application/wasm"),
            ("notes.txt", "text/plain; charset=utf-8"),
            ("vendor/xterm/LICENSE", "text/plain; charset=utf-8"),
            ("blob.unknownext", "application/octet-stream"),
        ] {
            let actual = content_type(name);
            // mime_guess owns the long tail; only the prefix of its answer matters there.
            assert!(
                actual == expected || (name == "favicon.ico" && actual.starts_with("image/")),
                "{name}: {actual} != {expected}"
            );
        }
    }

    #[test]
    fn the_root_maps_to_the_index_page() {
        assert_eq!(embedded_path("/").as_deref(), Some(INDEX));
        assert_eq!(embedded_path("/index.html").as_deref(), Some("index.html"));
        assert_eq!(
            embedded_path("/vendor/xterm/xterm.mjs").as_deref(),
            Some("vendor/xterm/xterm.mjs")
        );
    }

    #[test]
    fn paths_that_could_escape_the_folder_are_refused() {
        for path in [
            "",
            "index.html",
            "/../Cargo.toml",
            "/vendor/../../Cargo.toml",
            "/%2e%2e/Cargo.toml",
            "/vendor/%2E%2E/%2e%2e/Cargo.toml",
            "/vendor%2f..%2f..%2fCargo.toml",
            "//etc/passwd",
            "/vendor//xterm",
            "/vendor/",
            "/./index.html",
            "/vendor\\xterm\\xterm.mjs",
            "/%00",
            "/a%0ab",
        ] {
            assert_eq!(embedded_path(path), None, "{path:?}");
            assert!(lookup(path).is_none(), "{path:?}");
        }
    }

    #[test]
    fn a_vendored_module_is_served_with_its_type_and_a_stable_etag() {
        let asset = lookup("/vendor/xterm/xterm.mjs").expect("vendored xterm module");
        assert_eq!(asset.content_type, "text/javascript; charset=utf-8");
        assert!(!asset.data.is_empty());
        assert!(asset.etag.starts_with('"') && asset.etag.ends_with('"') && asset.etag.len() == 18);
        assert_eq!(lookup("/vendor/xterm/xterm.mjs").unwrap().etag, asset.etag);
        let other = lookup("/vendor/xterm/xterm.css").expect("vendored xterm css");
        assert_eq!(other.content_type, "text/css; charset=utf-8");
        assert_ne!(other.etag, asset.etag);
        let licence = lookup("/vendor/dockview-core/LICENSE").expect("vendored licence");
        assert_eq!(licence.content_type, "text/plain; charset=utf-8");
    }

    /// The entry page links its stylesheets by relative path. A stylesheet that moved while its
    /// `<link>` did not would silently leave the page unstyled, so each one must be served as CSS.
    #[test]
    fn every_stylesheet_the_entry_page_links_is_served_as_css() {
        let page = lookup("/").expect("the entry page");
        let html = String::from_utf8(page.data.into_owned()).expect("the entry page is UTF-8");
        let hrefs: Vec<&str> = html
            .split("<link")
            .skip(1)
            .filter_map(|tag| tag.split('>').next())
            .filter(|tag| tag.contains(r#"rel="stylesheet""#))
            .filter_map(|tag| tag.split(r#"href=""#).nth(1)?.split('"').next())
            .collect();
        assert!(
            hrefs.contains(&"vendor/dockview-core/dockview.css"),
            "the dockview stylesheet is not linked: {hrefs:?}"
        );
        for href in hrefs {
            let asset = lookup(&format!("/{href}"))
                .unwrap_or_else(|| panic!("{href} is linked but not served"));
            assert_eq!(asset.content_type, "text/css; charset=utf-8", "{href}");
            assert!(!asset.data.is_empty(), "{href} is empty");
        }
        // It lives with the library now; nothing is left at the old location.
        assert!(lookup("/dockview.css").is_none());
    }

    #[test]
    fn a_percent_encoded_path_reaches_the_same_file() {
        let plain = lookup("/vendor/xterm/xterm.mjs").unwrap();
        let encoded = lookup("/vendor/xterm/xterm%2Emjs").unwrap();
        assert_eq!(plain.etag, encoded.etag);
    }

    #[test]
    fn declaration_files_are_never_served() {
        for path in [
            "/vendor/addon-fit/addon-fit.d.ts",
            "/vendor/addon-fit/ADDON-FIT.D.TS",
            "/vendor/dockview-core/types/api/component.api.d.ts",
        ] {
            assert!(lookup(path).is_none(), "{path}");
        }
        // The sibling module is still there, so the lookup above is not failing for another reason.
        assert!(lookup("/vendor/addon-fit/addon-fit.mjs").is_some());
    }

    /// CONTRACT §7.3: vendored type definitions live outside `web/` (in `vendor-types/`), so none
    /// is embedded; everything that is embedded is served.
    #[test]
    fn no_declaration_file_is_embedded_and_every_embedded_file_is_served() {
        let mut files = 0;
        for name in WebAssets::iter() {
            files += 1;
            assert!(
                !is_hidden(&name),
                "{name} is embedded but would never be served"
            );
            assert!(
                lookup(&format!("/{name}")).is_some(),
                "{name} is not served"
            );
        }
        assert!(files > 0, "the web tree is empty");
    }

    /// The filter stays as a second line of defence: nothing embedded matches it any more, so it
    /// is tested directly.
    #[test]
    fn declaration_files_are_hidden_in_any_case() {
        for name in [
            "vendor/addon-fit/addon-fit.d.ts",
            "vendor/addon-fit/ADDON-FIT.D.TS",
            "a.d.ts",
        ] {
            assert!(is_hidden(name), "{name}");
        }
        for name in [
            "vendor/addon-fit/addon-fit.mjs",
            "types.ts",
            "d.ts.map",
            "x.d.tsx",
        ] {
            assert!(!is_hidden(name), "{name}");
        }
    }

    #[test]
    fn unknown_files_and_directories_are_missing() {
        assert!(lookup("/nope.js").is_none());
        assert!(lookup("/vendor").is_none());
        assert!(lookup("/vendor/xterm").is_none());
    }
}
