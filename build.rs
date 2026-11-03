//! Compiles resources/cajole.gresource.xml into a .gresource blob with
//! `glib-compile-resources`. The blob is registered at startup in main.rs.
//!
//! It holds the alarm-list badge icons (sound, vibration, glow, Gentle Mode).
//! They are internal UI icons and stay out of the hicolor tree.
//!
//! It also checks that the newest `<release>` in the metainfo matches the
//! `Cargo.toml` version. A mismatch fails release builds and warns otherwise.

const METAINFO: &str = "data/net.dunevent.Cajole.metainfo.xml";

/// The value of attribute `name` in the text of one start tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let len = tag[start..].find('"')?;
    Some(tag[start..start + len].to_string())
}

/// The version of the `<release>` with the latest `date`. The first one wins
/// a tie, so a newest-first list works as written.
fn newest_release_version(xml: &str) -> Option<String> {
    xml.split("<release")
        .skip(1)
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .filter_map(|rest| {
            let tag = &rest[..rest.find('>')?];
            Some((attribute(tag, "date").unwrap_or_default(), attribute(tag, "version")?))
        })
        .reduce(|best, next| if next.0 > best.0 { next } else { best })
        .map(|(_, version)| version)
}

/// Compares the metainfo's newest release with the crate version. Skipped
/// when the metainfo is absent, as in a packaged crate.
fn check_metainfo_version() {
    println!("cargo:rerun-if-changed={METAINFO}");
    let Ok(xml) = std::fs::read_to_string(METAINFO) else {
        return;
    };
    let crate_version = env!("CARGO_PKG_VERSION");
    let newest = newest_release_version(&xml);
    if newest.as_deref() == Some(crate_version) {
        return;
    }
    let message = format!(
        "Cargo.toml version is {crate_version} but the newest <release> in {METAINFO} is {}; \
         add a <release version=\"{crate_version}\" date=\"YYYY-MM-DD\"/> entry",
        newest.as_deref().unwrap_or("missing"),
    );
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        panic!("{message}");
    }
    println!("cargo:warning={message}");
}

fn main() {
    check_metainfo_version();

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set by cargo");
    let out_path = std::path::Path::new(&out_dir).join("cajole.gresource");

    let status = std::process::Command::new("glib-compile-resources")
        .arg("--sourcedir=resources")
        .arg(format!("--target={}", out_path.display()))
        .arg("resources/cajole.gresource.xml")
        .status()
        .expect("failed to run glib-compile-resources -- is it on PATH?");

    if !status.success() {
        panic!("glib-compile-resources failed with status: {status}");
    }

    println!("cargo:rerun-if-changed=resources/cajole.gresource.xml");
    println!("cargo:rerun-if-changed=resources/icons/sentiment-satisfied-symbolic.svg");
    println!("cargo:rerun-if-changed=resources/icons/music-note-outline-symbolic.svg");
    println!("cargo:rerun-if-changed=resources/icons/phone-vibrate-symbolic.svg");
    println!("cargo:rerun-if-changed=resources/icons/image-adjust-brightness-symbolic.svg");
}
