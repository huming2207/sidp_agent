//! Icons: GPUI Kit's default set plus the few extra Lucide icons this app
//! uses, embedded so that nothing is loaded at run time.
use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// Extra icons, by their `gpui_kit::assets::IconName` names.
pub use gpui_kit::assets::IconName as Lucide;

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [Microchip, Bug, Activity, CirclePlay, CircleStop, Download]
);

/// The app's asset source.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
