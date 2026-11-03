use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where a sound entry came from.
///
/// `Custom` entries are files a person added, copied into the app's sounds
/// directory when added (see [`SoundLibrary::add_custom`]), so moving or
/// deleting the original cannot break an alarm that references it.
///
/// `Default` entries are the built-in catalog from several open-source
/// projects (see [`SoundLibrary::default_catalog`]). Each needs an explicit
/// download before it is playable, including "Gentle", which was once bundled
/// in the binary. A pre-installed sound would undermine requiring an explicit
/// choice before an alarm can be created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SoundOrigin {
    Custom,
    Default {
        /// The project this came from, e.g. `"GNOME Clocks"`, `"AOSP"`,
        /// `"KDE Plasma (Ocean Sound Theme)"`.
        project: String,
        /// Credit line shown in the info popover: creator and license, as the
        /// project's credits file attributes it.
        attribution: String,
        /// Short license identifier, e.g. `"CC0-1.0"`, `"CC-BY-4.0"`,
        /// `"Apache-2.0"`, `"CC-BY-SA-4.0"`.
        license: String,
        /// Where to download the audio file.
        source_url: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundEntry {
    pub id: Uuid,
    pub label: String,
    pub origin: SoundOrigin,
    /// Local path once downloaded (`Default`) or added (`Custom`). `None`
    /// means a default not yet downloaded: its Preview is disabled and its
    /// Download button fills this in.
    pub file_path: Option<String>,
    /// Hides an entry without forgetting it. Meaningful for `Default` entries,
    /// which [`SoundLibrary::reset_defaults`] can restore. A removed `Custom`
    /// entry is gone, so [`SoundLibrary::remove`] deletes it outright instead
    /// of setting this flag.
    #[serde(default)]
    pub removed: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LibraryFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    entries: Vec<SoundEntry>,
}

fn default_version() -> u32 {
    1
}

pub struct SoundLibrary {
    path: PathBuf,
    entries: Vec<SoundEntry>,
}

impl SoundLibrary {
    /// Loads the library from `path`, seeding it with the default catalog on
    /// first run.
    pub fn load(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();

        if !path.exists() {
            let library = SoundLibrary {
                path,
                entries: Self::default_catalog(),
            };
            library.save()?;
            return Ok(library);
        }

        let contents = fs::read_to_string(&path)?;
        let file: LibraryFile = serde_json::from_str(&contents)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(SoundLibrary {
            path,
            entries: file.entries,
        })
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = LibraryFile {
            version: 1,
            entries: self.entries.clone(),
        };
        let serialized = serde_json::to_string_pretty(&file)
            .expect("SoundLibrary serialization cannot fail");
        let tmp_path = {
            let mut p = self.path.clone();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "sounds".to_string());
            p.set_file_name(format!("{name}.tmp"));
            p
        };
        fs::write(&tmp_path, serialized)?;
        fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    pub fn entries(&self) -> &[SoundEntry] {
        &self.entries
    }

    /// Entries a person can see: all except removed ones.
    pub fn visible_entries(&self) -> impl Iterator<Item = &SoundEntry> {
        self.entries.iter().filter(|e| !e.removed)
    }

    /// Entries with a local file, which the alarm form's sound picker offers;
    /// a not-yet-downloaded default cannot be played.
    pub fn available_entries(&self) -> impl Iterator<Item = &SoundEntry> {
        self.visible_entries().filter(|e| e.file_path.is_some())
    }

    /// Visible entries in display order: custom sounds, then downloaded
    /// defaults, then not-yet-downloaded defaults, each group alphabetical by
    /// label (case-insensitive). The Sound Manager's rows and the alarm form's
    /// picker use this order instead of [`SoundLibrary::entries`]'s insertion
    /// order.
    pub fn sorted_visible_entries(&self) -> Vec<&SoundEntry> {
        fn group_rank(entry: &SoundEntry) -> u8 {
            match (&entry.origin, entry.file_path.is_some()) {
                (SoundOrigin::Custom, _) => 0,
                (SoundOrigin::Default { .. }, true) => 1,
                (SoundOrigin::Default { .. }, false) => 2,
            }
        }
        let mut entries: Vec<&SoundEntry> = self.visible_entries().collect();
        entries.sort_by(|a, b| {
            group_rank(a)
                .cmp(&group_rank(b))
                .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
        });
        entries
    }

    pub fn get(&self, id: Uuid) -> Option<&SoundEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Whether any default entry is hidden, which enables "Restore Defaults"
    /// in `main.rs`.
    pub fn has_removed_defaults(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.removed && matches!(e.origin, SoundOrigin::Default { .. }))
    }

    /// Registers a custom sound already copied to `local_path` by `main.rs`'s
    /// add-custom-sound flow. Only records it; does not touch the filesystem.
    pub fn add_custom(&mut self, label: String, local_path: String) -> io::Result<Uuid> {
        let id = Uuid::new_v4();
        self.entries.push(SoundEntry {
            id,
            label,
            origin: SoundOrigin::Custom,
            file_path: Some(local_path),
            removed: false,
        });
        self.save()?;
        Ok(id)
    }

    /// Records that a default entry finished downloading to `local_path`.
    pub fn mark_downloaded(&mut self, id: Uuid, local_path: String) -> io::Result<()> {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.file_path = Some(local_path);
        }
        self.save()
    }

    /// Deletes a downloaded default's file and marks it not downloaded,
    /// without hiding it: it stays visible in the not-downloaded group. This
    /// is the first step of the Sound Manager's two-stage remove; the second
    /// hides it.
    pub fn clear_download(&mut self, id: Uuid) -> io::Result<()> {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            if let Some(path) = entry.file_path.take() {
                let _ = fs::remove_file(path);
            }
        }
        self.save()
    }

    /// Removes an entry. A `Custom` entry is deleted outright, including its
    /// file (a missing file is not an error). A `Default` entry is hidden
    /// (`removed = true`) and its downloaded file deleted, so
    /// [`SoundLibrary::reset_defaults`] can restore it.
    pub fn remove(&mut self, id: Uuid) -> io::Result<()> {
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else {
            return Ok(());
        };
        if let Some(path) = entry.file_path.take() {
            let _ = fs::remove_file(path);
        }
        match &entry.origin {
            SoundOrigin::Custom => {
                self.entries.retain(|e| e.id != id);
            }
            SoundOrigin::Default { .. } => {
                entry.removed = true;
            }
        }
        self.save()
    }

    /// Restores every removed default and adds catalog entries introduced by a
    /// newer Cajole that this library file predates. Downloads stay an
    /// explicit per-sound action.
    pub fn reset_defaults(&mut self) -> io::Result<()> {
        for entry in &mut self.entries {
            if matches!(entry.origin, SoundOrigin::Default { .. }) {
                entry.removed = false;
            }
        }
        let known_projects_and_labels: std::collections::HashSet<(String, String)> = self
            .entries
            .iter()
            .filter_map(|e| match &e.origin {
                SoundOrigin::Default { project, .. } => Some((project.clone(), e.label.clone())),
                SoundOrigin::Custom => None,
            })
            .collect();
        for catalog_entry in Self::default_catalog() {
            let SoundOrigin::Default { project, .. } = &catalog_entry.origin else {
                continue;
            };
            let key = (project.clone(), catalog_entry.label.clone());
            if !known_projects_and_labels.contains(&key) {
                self.entries.push(catalog_entry);
            }
        }
        self.save()
    }

    /// The built-in default catalog: alarm sounds from four open-source projects, each
    /// license-checked against its own credits or license files.
    ///
    /// - GNOME Clocks: CC0 and CC-BY 3.0/4.0 sounds from its `data/sounds/CREDITS`
    ///   file. `digitone` has no listed license and is excluded.
    /// - AOSP: Apache-2.0, per `frameworks/base`'s `MODULE_LICENSE_APACHE2` marker
    ///   and `NOTICE` file (no contradicting per-file license found).
    /// - KDE Plasma (Ocean Sound Theme): CC-BY-SA-4.0, per the `.oga.license`
    ///   sidecar file next to the sound.
    /// - KDE Plasma Mobile: CC0, CC-BY-4.0 and CC-BY-SA-4.0 sounds per the README
    ///   table (<https://invent.kde.org/plasma/plasma-mobile-sounds>) and its
    ///   `LICENSES/` directory. Only "Ringtone/Alarm" sounds are included, not
    ///   "Notification" ones. "Gentle" (CC-BY-4.0, Derek Lin) needs the same
    ///   explicit download as the rest.
    pub fn default_catalog() -> Vec<SoundEntry> {
        let mut entries = Vec::new();

        let gnome_clocks_cc0 = [
            ("Bird", "bird", "unfa"),
            ("Bonfire", "bonfire", "Jordan Daniel Mills"),
            ("Bouncing", "bouncing", "Berivan Kilavuz"),
            ("Daydreaming", "daydreaming", "MarcosPurziani"),
            ("Glass Bell", "glass-bell", "zozu"),
            ("Mystery", "mystery", "PhonosUPF"),
            ("Ping Ping", "ping-ping", "Mattias \"Mattix\" Lahoud"),
            ("School Bell", "school-bell", "melokacool"),
            ("Sonar", "sonar", "assembly.diskette"),
            ("Tingling", "tingling", "beckmen"),
        ];
        for (label, file, creator) in gnome_clocks_cc0 {
            entries.push(default_entry(
                label,
                "GNOME Clocks",
                &format!("{creator}, via freesound.org"),
                "CC0-1.0",
                &format!("https://raw.githubusercontent.com/GNOME/gnome-clocks/master/data/sounds/alarms/{file}.flac"),
            ));
        }

        let gnome_clocks_cc_by_4 = [
            ("Beep Beep", "beep-beep", "Tim/corsica_s"),
            ("Boudoir", "boudoir", "skylar.mianlind"),
            ("Cautious Steps", "cautious-steps", "ImATaco"),
            ("Class Act", "class-act", "knzz.io"),
            ("Crossing Bell", "crossing-bell", "WeenyBeany"),
            ("Enchanting", "enchanting", "jerry.berumen"),
            ("Guitar", "guitar", "Valentin Sosnitskiy"),
            ("Jolly", "jolly", "Jerry Berumen"),
            ("Sparkling", "sparkling", "nlux"),
        ];
        for (label, file, creator) in gnome_clocks_cc_by_4 {
            entries.push(default_entry(
                label,
                "GNOME Clocks",
                &format!("{creator}, via freesound.org (CC-BY 4.0 -- attribution required)"),
                "CC-BY-4.0",
                &format!("https://raw.githubusercontent.com/GNOME/gnome-clocks/master/data/sounds/alarms/{file}.flac"),
            ));
        }

        let gnome_clocks_cc_by_3 = [("Chirping", "chirping", "djfroyd"), ("Toys", "toys", "Thirsk")];
        for (label, file, creator) in gnome_clocks_cc_by_3 {
            entries.push(default_entry(
                label,
                "GNOME Clocks",
                &format!("{creator}, via freesound.org (CC-BY 3.0 -- attribution required)"),
                "CC-BY-3.0",
                &format!("https://raw.githubusercontent.com/GNOME/gnome-clocks/master/data/sounds/alarms/{file}.flac"),
            ));
        }

        let aosp_elements = [
            "Argon", "Carbon", "Cesium", "Helium", "Krypton", "Neon", "Osmium", "Oxygen",
            "Platinum", "Barium",
        ];
        for name in aosp_elements {
            entries.push(default_entry(
                name,
                "AOSP",
                "The Android Open Source Project",
                "Apache-2.0",
                &format!("https://raw.githubusercontent.com/aosp-mirror/platform_frameworks_base/master/data/sounds/alarms/ogg/{name}.ogg"),
            ));
        }

        entries.push(default_entry(
            "Ocean",
            "KDE Plasma (Ocean Sound Theme)",
            "Guilherme Marcal Silva (CC-BY-SA 4.0 -- attribution required)",
            "CC-BY-SA-4.0",
            "https://raw.githubusercontent.com/KDE/ocean-sound-theme/master/ocean/stereo/alarm-clock-elapsed.oga",
        ));

        // Plasma Mobile's filenames match these labels (unlike GNOME Clocks'
        // slugified ones), so the URL is the label plus ".oga", with spaces
        // percent-encoded.
        let plasma_mobile_cc0 = [
            ("Glazed", "Ivan Kiselyov"),
            ("Lightly", "Ivan Kiselyov"),
            ("Miniature", "Ivan Kiselyov"),
            ("On The Way", "Ivan Kiselyov"),
        ];
        for (label, creator) in plasma_mobile_cc0 {
            let file = label.replace(' ', "%20");
            entries.push(default_entry(
                label,
                "KDE Plasma Mobile",
                &format!("{creator}, via KDE Plasma Mobile Sounds"),
                "CC0-1.0",
                &format!("https://raw.githubusercontent.com/KDE/plasma-mobile-sounds/master/plasma-mobile/stereo/ringtones/{file}.oga"),
            ));
        }

        let plasma_mobile_cc_by_4 = [
            ("Arp", "Shell Raiser"),
            ("Crunch", "Neyrax"),
            ("Errand", "Axel Lopez"),
            ("Gentle", "Derek Lin"),
            ("Machine", "Neyrax"),
            ("Morning Rush", "Neyrax"),
            ("Progressive", "Neyrax"),
            ("Spatial", "Axel Lopez"),
            ("Unfamiliar", "Shell Raiser"),
            ("Wink", "Derek Lin"),
            ("Wubstep", "Axel Lopez"),
        ];
        for (label, creator) in plasma_mobile_cc_by_4 {
            let file = label.replace(' ', "%20");
            entries.push(default_entry(
                label,
                "KDE Plasma Mobile",
                &format!("{creator} (CC-BY 4.0 -- attribution required)"),
                "CC-BY-4.0",
                &format!("https://raw.githubusercontent.com/KDE/plasma-mobile-sounds/master/plasma-mobile/stereo/ringtones/{file}.oga"),
            ));
        }

        let plasma_mobile_cc_by_sa_4 = [
            ("Elevated", "nesdood007"),
            ("Nightlights", "nesdood007"),
            ("Revelation", "nesdood007"),
            ("Slowly", "kde-qnebra"),
        ];
        for (label, creator) in plasma_mobile_cc_by_sa_4 {
            let file = label.replace(' ', "%20");
            entries.push(default_entry(
                label,
                "KDE Plasma Mobile",
                &format!("{creator} (CC-BY-SA 4.0 -- attribution required)"),
                "CC-BY-SA-4.0",
                &format!("https://raw.githubusercontent.com/KDE/plasma-mobile-sounds/master/plasma-mobile/stereo/ringtones/{file}.oga"),
            ));
        }

        entries
    }
}

fn default_entry(label: &str, project: &str, attribution: &str, license: &str, source_url: &str) -> SoundEntry {
    SoundEntry {
        id: Uuid::new_v4(),
        label: label.to_string(),
        origin: SoundOrigin::Default {
            project: project.to_string(),
            attribution: attribution.to_string(),
            license: license.to_string(),
            source_url: source_url.to_string(),
        },
        file_path: None,
        removed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("cajole-sounds-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fresh_library_is_seeded_with_the_default_catalog() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let library = SoundLibrary::load(&path).unwrap();
        assert!(!library.entries().is_empty());
        assert!(library
            .entries()
            .iter()
            .all(|e| matches!(e.origin, SoundOrigin::Default { .. })));
        // None are downloaded yet by default.
        assert_eq!(library.available_entries().count(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_catalog_covers_all_three_sources() {
        let catalog = SoundLibrary::default_catalog();
        let projects: std::collections::HashSet<&str> = catalog
            .iter()
            .filter_map(|e| match &e.origin {
                SoundOrigin::Default { project, .. } => Some(project.as_str()),
                _ => None,
            })
            .collect();
        assert!(projects.contains("GNOME Clocks"));
        assert!(projects.contains("AOSP"));
        assert!(projects.iter().any(|p| p.starts_with("KDE Plasma")));
    }

    #[test]
    fn add_custom_persists_and_is_immediately_available() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let mut library = SoundLibrary::load(&path).unwrap();
        let id = library
            .add_custom("My Recording".to_string(), "/fake/path.ogg".to_string())
            .unwrap();

        assert!(library.get(id).is_some());
        assert_eq!(library.available_entries().count(), 1);

        let reloaded = SoundLibrary::load(&path).unwrap();
        assert!(reloaded.get(id).is_some());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removing_a_custom_entry_deletes_it_outright() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let mut library = SoundLibrary::load(&path).unwrap();
        let id = library
            .add_custom("Temp".to_string(), "/fake/path.ogg".to_string())
            .unwrap();
        library.remove(id).unwrap();
        assert!(library.get(id).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removing_a_default_entry_hides_rather_than_deletes_it() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let mut library = SoundLibrary::load(&path).unwrap();
        let id = library.entries()[0].id;

        library.remove(id).unwrap();
        assert!(library.get(id).is_some(), "entry should still exist");
        assert!(library.get(id).unwrap().removed);
        assert_eq!(
            library.visible_entries().count(),
            library.entries().len() - 1
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reset_defaults_unhides_removed_default_entries() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let mut library = SoundLibrary::load(&path).unwrap();
        let id = library.entries()[0].id;
        library.remove(id).unwrap();
        assert!(library.get(id).unwrap().removed);

        library.reset_defaults().unwrap();
        assert!(!library.get(id).unwrap().removed);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mark_downloaded_makes_an_entry_available() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let mut library = SoundLibrary::load(&path).unwrap();
        let id = library.entries()[0].id;
        // Nothing downloaded/extracted yet, including "Gentle" (see
        // `fresh_library_is_seeded_with_the_default_catalog`).
        assert_eq!(library.available_entries().count(), 0);

        library
            .mark_downloaded(id, "/some/local/path.flac".to_string())
            .unwrap();
        assert_eq!(library.available_entries().count(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sorted_visible_entries_groups_custom_before_downloaded_before_undownloaded() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let mut library = SoundLibrary::load(&path).unwrap();

        // Everything starts undownloaded, "Gentle" included. One default is
        // downloaded and one custom sound added so all three groups exist,
        // with two entries in the available group.
        let some_default_id = library.entries()[0].id;
        library
            .mark_downloaded(some_default_id, "/fake/downloaded.flac".to_string())
            .unwrap();
        let custom_id = library
            .add_custom("Zzz Custom".to_string(), "/fake/custom.ogg".to_string())
            .unwrap();

        let sorted = library.sorted_visible_entries();
        // The custom sound is first despite sorting last by label: group takes
        // priority.
        assert_eq!(sorted[0].id, custom_id);
        // The one downloaded default comes next...
        assert_eq!(sorted[1].id, some_default_id);
        // ...then every remaining (undownloaded) default.
        assert!(sorted[2..]
            .iter()
            .all(|e| matches!(e.origin, SoundOrigin::Default { .. }) && e.file_path.is_none()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sorted_visible_entries_are_alphabetical_within_each_group() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let library = SoundLibrary::load(&path).unwrap();
        // Right after a fresh load every `Default` entry, "Gentle" included,
        // is undownloaded, so the list is one group and fully alphabetical
        // (case-insensitive).
        let sorted = library.sorted_visible_entries();
        let labels: Vec<String> = sorted.iter().map(|e| e.label.to_lowercase()).collect();
        let mut expected = labels.clone();
        expected.sort();
        assert_eq!(labels, expected);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_download_demotes_without_hiding() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let mut library = SoundLibrary::load(&path).unwrap();
        let id = library.entries()[0].id;
        library
            .mark_downloaded(id, "/fake/path.flac".to_string())
            .unwrap();
        assert_eq!(library.available_entries().count(), 1);

        library.clear_download(id).unwrap();
        assert!(library.get(id).unwrap().file_path.is_none());
        assert!(!library.get(id).unwrap().removed, "should still be visible");
        assert_eq!(library.available_entries().count(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn has_removed_defaults_reflects_current_state() {
        let dir = tempdir();
        let path = dir.join("sounds.json");
        let mut library = SoundLibrary::load(&path).unwrap();
        assert!(!library.has_removed_defaults());

        let id = library.entries()[0].id;
        library.remove(id).unwrap();
        assert!(library.has_removed_defaults());

        library.reset_defaults().unwrap();
        assert!(!library.has_removed_defaults());
        fs::remove_dir_all(&dir).ok();
    }
}
