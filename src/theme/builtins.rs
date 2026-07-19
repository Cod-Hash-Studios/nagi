use std::sync::OnceLock;

use crate::app::state::Palette;

pub(crate) fn source(name: &str) -> Option<&'static str> {
    match name {
        "nagi-night" => Some(include_str!("../../assets/themes/nagi-night.toml")),
        "nagi-dawn" => Some(include_str!("../../assets/themes/nagi-dawn.toml")),
        _ => None,
    }
}

pub(crate) fn palette(name: &str) -> Option<Palette> {
    static NIGHT: OnceLock<Option<Palette>> = OnceLock::new();
    static DAWN: OnceLock<Option<Palette>> = OnceLock::new();

    let cell = match name {
        "nagi-night" => &NIGHT,
        "nagi-dawn" => &DAWN,
        _ => return None,
    };
    cell.get_or_init(|| {
        source(name).and_then(|source| {
            crate::theme::loader::load_manifest_str(source, name)
                .ok()
                .map(|loaded| loaded.palette)
        })
    })
    .clone()
}
