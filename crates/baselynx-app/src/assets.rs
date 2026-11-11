//! The toolkit's component icons plus the few extra Lucide icons baselynx uses.

use std::borrow::Cow;

use gpui_kit::assets::icon_assets;
use gpui_kit::*;

icon_assets!(
    ExtraIcons,
    [
        KeyRound,
        Table,
        Download,
        Database,
        SunMoon,
        Clock,
        Lock,
        Pin,
        PinOff,
        ArrowUpRight,
        Server,
        ListFilter,
        ChartNoAxesColumn,
        Braces,
        Link,
        Rows3,
        CircleCheck,
        CircleX,
        TriangleAlert
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(found) = ExtraIcons.load(path)? {
            return Ok(Some(found));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut all = gpui_kit::assets::Assets.list(path)?;
        all.extend(ExtraIcons.list(path)?);
        Ok(all)
    }
}
