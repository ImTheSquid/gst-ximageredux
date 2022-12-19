use gst::glib;
pub mod ximageredux;
pub use crate::ximageredux::*;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    ximageredux::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    ximageredux,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, glib::Enum, Default)]
#[enum_type(name = "GstXImageReduxWindowVisibility")]
#[repr(i32)]
pub enum WindowVisibility {
    #[default]
    Unknown = 0,
    Visible = 1,
    Hidden = 2
}