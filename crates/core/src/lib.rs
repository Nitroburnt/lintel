//! notch-core: what every feature builds against (docs/ARCHITECTURE.md §2, §6, §8).

pub mod config_file;
pub mod feature;
pub mod files;
pub mod i18n;
pub mod log;
pub mod migrate;
pub mod net;
pub mod secrets;
pub mod storage;
pub mod win;

pub use feature::{
    paused, set_paused, Anchor, Ctx, Feature, Group, Host, Manifest, Monitor, ShortcutDef, Use, WindowFactory,
    WindowSpec,
};
