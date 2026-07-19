use std::{collections::BTreeSet, fs, path::PathBuf};

const SIZES: &[(u16, u16)] = &[(60, 20), (80, 24), (120, 32), (200, 48)];
const THEMES: &[&str] = &["nagi-night", "nagi-dawn", "terminal-16", "custom-ume"];
const SURFACES: &[&str] = &[
    "terminal",
    "sessions-1",
    "sessions-8",
    "sessions-50",
    "sessions-500",
    "mission-cockpit",
    "command-palette",
    "settings",
    "mission-inspector",
    "proof-review",
    "attention-inbox",
];

#[test]
fn golden_inventory_is_complete_and_reviewable() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let expected = SURFACES
        .iter()
        .flat_map(|surface| {
            THEMES.iter().flat_map(move |theme| {
                SIZES
                    .iter()
                    .map(move |(width, height)| format!("{surface}__{theme}__{width}x{height}.txt"))
            })
        })
        .collect::<BTreeSet<_>>();
    let actual = fs::read_dir(&root)
        .expect("tests/golden must exist; run scripts/render_ui_goldens.py --update")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".txt"))
        .collect::<BTreeSet<_>>();

    assert_eq!(actual, expected, "golden inventory drifted");
    for name in expected {
        let snapshot = fs::read_to_string(root.join(&name)).unwrap();
        assert!(snapshot.starts_with("# nagi-ui-golden v1\n"), "{name}");
        assert!(snapshot.contains("# style-sha256: "), "{name}");
        assert!(snapshot.contains("\n# ---\n"), "{name}");
    }
}
